use super::claude_stream::{ClaudeMetrics, ClaudeOutcome, interpret_claude_output, is_claude_init};
use super::stream::{Capture, Observer, read_stream};
use super::*;

type SharedClaudeMetrics = std::sync::Arc<std::sync::Mutex<ClaudeMetrics>>;

const CLAUDE_INIT_SCAN_MAX: usize = 1 << 20;
const CLAUDE_TAIL_KEEP: usize = 8 << 20;

struct ClaudeObserver {
    metrics: SharedClaudeMetrics,
    lease: Lease,
}

impl Observer for ClaudeObserver {
    fn line_cap(&self) -> Option<usize> {
        Some(STDOUT_CAPTURE_CAP)
    }

    fn line(&mut self, line: &[u8]) {
        if !self.lease.is_ready() && line.len() <= CLAUDE_INIT_SCAN_MAX && is_claude_init(line) {
            self.lease.ready();
        }
        if let Ok(mut metrics) = self.metrics.lock() {
            metrics.record_line(line);
        }
    }
}

/// How long `review` lets a claude shell command run in the foreground: 20
/// minutes, the allowance grok gets (`GROK_COMMAND_WAIT_SECS`) for the same
/// reason - long enough for a full gate, short enough that a wedged command
/// cannot hold a run for hours. Set as both `BASH_DEFAULT_TIMEOUT_MS` and
/// `BASH_MAX_TIMEOUT_MS`; see `run_claude` for why the default is the one that
/// matters.
const CLAUDE_COMMAND_WAIT_SECS: u64 = 20 * 60;

pub async fn run_claude(
    launch: &super::LaunchSpec,
    launched: Option<LaunchSignal>,
    runtime: &ProviderRuntime,
) -> Result<RunOutput> {
    let session = &launch.start;
    let selection = &launch.settings.selection;
    let env = launch.settings.env.as_ref();
    let prompt = launch.prompt.as_str();
    let project_root = launch.root.as_path();

    // The invocation owner supplies a fresh UUID; runners never mint IDs.
    // Entry validation also protects direct runner callers.
    selection.validate(Provider::Claude)?;
    session.validate(Provider::Claude)?;
    let session_id = session
        .id()
        .expect("validated caller-minted identity")
        .as_str();
    let fresh_id = match session {
        SessionStart::Fresh { id } => id.as_ref().map(|id| id.as_str().to_string()),
        SessionStart::Resume(_) => None,
    };

    let mut args: Vec<String> = if let Some(ref id) = fresh_id {
        vec!["--session-id".to_string(), id.clone()]
    } else {
        vec!["--resume".to_string(), session_id.to_string()]
    };
    // The stream ends in one result object per run, whatever the outcome,
    // carrying `is_error` - so the turn classifies itself, as grok's does, and a
    // failed run still names its session. Text output gave neither: a non-zero
    // exit was all `review` saw, and it discarded the session id with it.
    //
    // `stream-json` rather than `json` (which prints only that object) for its
    // first event: an `init` within half a second of launch, which is what
    // makes claude safe to signal (see the wait loop below). `--verbose` is
    // required with it under `--print`. When claude wakes the model for a
    // background subagent's result, the stream carries a result per turn and the
    // last is the answer - moot while background tasks are disabled below.
    for arg in [
        "--print",
        "--permission-mode",
        "dontAsk",
        "--output-format",
        "stream-json",
        "--verbose",
    ] {
        args.push(arg.to_string());
    }
    args.extend_from_slice(selection.args());

    // Launched under a keeper (`contained::spawn`), which gives claude its own
    // process group and tears down every shell it started when the run ends.
    //
    // Keep the model's shell commands in the foreground, where their output
    // reaches it. `--print` does not wait for a background Bash command when the
    // model ends its turn: it *kills* it and exits 0. Probed on Claude Code
    // 2.1.284, asked only to run a 60s command in the background and report its
    // output, the model replied "I'll report exactly what it prints when it
    // finishes", ended its turn, and the command was killed seconds later - a
    // clean-looking run with no report, the loss grok's wake loop exists for.
    // Claude's switch removes the `run_in_background` parameter altogether, so
    // the same prompt ran the command in the foreground and reported its output.
    // (A background *subagent* was already waited for and its result handed
    // back in a second turn; it now just runs in the foreground too.)
    //
    // The cost of the switch is Bash's 2-minute default timeout, which kills a
    // longer foreground command outright. The model can pass its own `timeout`,
    // but under `dontAsk` a permission allowlist that names commands (like
    // `Bash(python3 *)`) denies any call carrying one - observed - so the
    // default is the limit that actually applies, and it is raised to
    // `CLAUDE_COMMAND_WAIT_SECS`: a 150s command then ran to completion. The
    // maximum is raised with it, for a model that is allowed to pass a timeout.
    // Set before the profile env so a profile can restate any of them.
    let wait_ms = (CLAUDE_COMMAND_WAIT_SECS * 1000).to_string();
    let spec = ProcessSpec::new(
        runtime.commands.get(Provider::Claude),
        args,
        project_root,
        &[
            ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1".to_string()),
            ("BASH_DEFAULT_TIMEOUT_MS", wait_ms.clone()),
            ("BASH_MAX_TIMEOUT_MS", wait_ms),
        ],
        env,
        &[],
        Stdin::Pipe(prompt.as_bytes().to_vec()),
    );
    // The lifecycle clears a stale interrupt request on a resume (one would
    // mark a failed resume as interrupted, hiding claude's stated reason), and
    // refuses to launch once `review` was told to stop: a launch then would
    // only be killed before its first event - and, a stopped run being
    // recorded as interrupted, file a row and print a resume command for a
    // session claude may never have written.
    //
    // The session id is known before the run starts - generated for a fresh
    // run, passed in for a resume - so the in-flight marker goes up before the
    // spawn. That ordering is load-bearing: `review message` releases its
    // session launch lock when the launch signal below fires, and a second
    // message must then find this marker, or it launches beside this run
    // instead of interrupting it. Removed on any early return below.
    //
    // The keeper's pid is offered on the marker for `review interrupt` only
    // once claude has printed its `init` event (`lease.ready()` in the
    // reader). A SIGINT before claude installs its handler takes the default
    // action and kills it with nothing printed; after, it ends the turn with a
    // result object.
    let marker_sid = fresh_id.clone().unwrap_or_else(|| session_id.to_string());
    let owner = Lease::known(
        &marker_sid,
        Provider::Claude,
        project_root,
        runtime.data_root.as_deref(),
    );
    let lease = Lease::clone(&owner);
    let (mut run, pipes) = Run::spawn(
        Provider::Claude,
        &spec,
        session,
        LeaseUse::Owned(owner),
        runtime,
    )?;

    let stdout_shared = new_shared_buf();
    let capture = Capture::head_tail(
        std::sync::Arc::clone(&stdout_shared),
        STDOUT_CAPTURE_CAP,
        CLAUDE_TAIL_KEEP,
    );
    let claude_metrics = std::sync::Arc::new(std::sync::Mutex::new(ClaudeMetrics::default()));
    let stderr_shared = new_shared_buf();
    let stdout_task = tokio::spawn(read_stream(
        pipes.stdout,
        capture.clone(),
        ClaudeObserver {
            metrics: std::sync::Arc::clone(&claude_metrics),
            lease: lease.clone(),
        },
    ));
    let stderr_task = tokio::spawn(read_capped(
        pipes.stderr,
        STDERR_CAPTURE_CAP,
        std::sync::Arc::clone(&stderr_shared),
    ));
    run.track(&stdout_task);
    run.track(&stderr_task);
    run.launch(launched).await?;

    // Wait for claude, but end the turn if `review` is signalled to stop.
    // Probed on Claude Code 2.1.284 mid-command: a SIGINT to claude ends the
    // turn at once, kills the running command, and still prints a result object
    // (`is_error: true`, `terminal_reason: "aborted_tools"`) naming the session,
    // which then resumes normally. A SIGTERM also leaves a resumable session but
    // prints nothing, so there would be no result to record. Before claude's
    // first event it may not have a handler yet, so then the group is killed
    // instead, as codex's is (`Run::wait`'s stop arm).
    run.wait(std::future::pending::<()>()).await;
    let ended = run.finish().await;
    let interrupt_requested = ended.interrupt_requested;
    let stopped_by_signal = ended.stopped_by_signal;
    let leftovers = ended.leftovers;
    // `Unknown` when the keeper could not report it. The run still goes on to
    // what claude printed - its answer and session survive an unknown
    // teardown, which the ledger already fails the command for.
    let status = ended.status;
    let _ = collect_reader(
        stdout_task,
        &stdout_shared,
        runtime.drain_grace,
        "claude stdout",
    )
    .await;
    let stdout_buf = capture.snapshot();
    let stderr_buf = collect_reader(
        stderr_task,
        &stderr_shared,
        runtime.drain_grace,
        "claude stderr",
    )
    .await;
    let write_res = ended.stdin;
    let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();

    // The echoed ID wins on a fresh run in case claude reassigned the requested
    // UUID; on resume, retain the ID the caller asked us to continue.
    let keep_id = |echoed: Option<String>| session.result_id(echoed);
    let metrics = claude_metrics
        .lock()
        .map(|metrics| metrics.clone())
        .unwrap_or_default();
    let served = metrics.served();
    let turns = metrics.turns;
    let usage = metrics.usage;
    let with_usage = |digest: Digest| Digest {
        turns,
        usage: usage.clone(),
        ..digest
    };
    // An abnormal end always keeps its observed status; a clean answer with a
    // clean exit records no digest, as for grok.
    let forensics = |outcome: &Outcome| {
        if outcome.is_clean() {
            answered_status_digest(status).map(with_usage)
        } else {
            Some(with_usage(status_digest(status)))
        }
    };
    // An interrupt explains the missing answer, so claude's own complaint
    // about being cut off is not kept as a stated reason; `ended` applies
    // "an answer wins over an interrupt request".
    let settle = |outcome: Outcome| {
        let outcome = outcome.ended(interrupt_requested, stopped_by_signal, None);
        if outcome.interrupted() {
            Outcome {
                stated: None,
                unverified: None,
                ..outcome
            }
        } else {
            outcome
        }
    };
    // Whether claude printed its first event, which carries the session id:
    // past that point the session exists (its transcript is on disk), so a run
    // that then ends without a result still has a session worth keeping.
    let started = lease.is_ready();

    let (text, outcome, session_id) = match interpret_claude_output("claude", &stdout, &stderr) {
        Ok(ClaudeOutcome::Answered { text, session_id }) => {
            (text, Outcome::answered(Answer::Captured), session_id)
        }
        Ok(ClaudeOutcome::NoAnswer {
            reason,
            review_error,
            text,
            session_id,
        }) => {
            let outcome = if review_error {
                Outcome::no_answer().unverified(reason)
            } else {
                Outcome::no_answer().stated(reason)
            };
            (text, outcome, session_id)
        }
        // No result, but claude had started: interrupted before it could
        // answer for itself, or died (killed from outside, crashed) after
        // starting. Either way the session exists, and dropping its id - as an
        // `Err` would, `invoke` setting none - makes the one turn most worth
        // resuming unresumable. A death is `Ok` with no stated reason, as for
        // codex, because claude stated none.
        // An interrupted one shows the interrupt text instead of the error.
        Err(e) if started => (
            if interrupt_requested {
                String::new()
            } else {
                format!("(claude ended without a result: {e})")
            },
            Outcome::no_answer(),
            None,
        ),
        // Stopped before claude's first event: the group was killed before
        // claude could have written anything, so there is no session to name.
        // Recording one would print a resume command that fails.
        Err(_) if stopped_by_signal => {
            anyhow::bail!("not started: review was signalled to stop before claude's first event");
        }
        // No result object and claude never started: the prompt may never
        // have arrived, which is the more useful thing to say when it didn't.
        Err(e) => match write_res {
            Err(write_err) => return Err(write_err.context(e.to_string())),
            Ok(()) => return Err(e),
        },
    };
    let outcome = settle(outcome);
    let text = if outcome.interrupted() && text.trim().is_empty() {
        outcome.placeholder(Provider::Claude)
    } else {
        text
    };
    Ok(RunOutput {
        text,
        digest: forensics(&outcome),
        outcome,
        session_id: keep_id(session_id),
        codex_home: None,
        served,
        leftovers,
    })
}

/// Sum claude's result-event metrics, mapped onto the codex-shaped `Usage`:
/// codex's `input_tokens` includes the cached part, while claude reports
/// uncached input, cache writes and cache reads separately, so all three are
/// summed and the reads are the cached share. Only result events contribute.
#[cfg(test)]
fn summarize_claude_output(stdout: &str) -> ClaudeMetrics {
    let mut metrics = ClaudeMetrics::default();
    for line in stdout.lines() {
        metrics.record_line(line.as_bytes());
    }
    metrics
}

#[cfg(test)]
fn claude_usage(stdout: &str) -> (u32, Usage) {
    let metrics = summarize_claude_output(stdout);
    (metrics.turns, metrics.usage)
}

#[cfg(test)]
fn claude_served(stdout: &str) -> Served {
    summarize_claude_output(stdout).served()
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;

/// Claude has no review-managed permission files to prepare.
pub(super) fn prepare(_launch: &super::LaunchSpec) {}
