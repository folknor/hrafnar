//! The Claude-compatible adapters: `claude -p` and `maki -p`, both with
//! stream-json output decoded by `claude_stream`. maki's headless mode
//! reproduces Claude Code's wire format, so one runner drives both and the
//! `Dialect` owns what differs: the argv, the forced environment, and (in
//! `claude_stream`) how the result and its cost are read. Wire compatibility
//! is not equal semantics, which is why the result reading is not shared.
//!
//! The rules both dialects run under:
//!
//! - The session id is minted by the invocation owner before dispatch, so the
//!   in-flight marker goes up before the spawn. The prompt goes on stdin.
//! - Neither has a filesystem sandbox `hrafnar` controls: claude's
//!   `--permission-mode` is a tool-approval policy, and maki has only its own
//!   permission files. Profiles for both must omit `sandbox`, non-empty
//!   `writable_roots` and non-empty `config` (config parsing refuses them),
//!   and a run records no sandbox level.
//! - The result object states the outcome, so `hrafnar` reads nothing from
//!   either harness's transcript and runs no stall watchdog: codex's watchdog
//!   rests on codex writing its rollout every few minutes, which nothing has
//!   established for these harnesses. There is no auto-resume either, because
//!   a failed turn says why.
//! - A first SIGINT ends the turn with a result and keeps the session, which
//!   `hrafnar interrupt`, `hrafnar message` and a signalled `hrafnar` rely on.
//!   Before the first event the group is killed instead.
//!
//! The measured harness behaviour behind these choices is in
//! `reference/claude.md` and `reference/maki.md`.

pub(super) use super::claude_stream::Dialect;
use super::claude_stream::{ClaudeMetrics, ClaudeOutcome, interpret_claude_output, is_claude_init};
use super::stream::{Capture, Observer, read_stream};
use super::*;

type SharedClaudeMetrics = std::sync::Arc<std::sync::Mutex<ClaudeMetrics>>;

/// The longest line checked for the `init` event; a longer one is not it.
const CLAUDE_INIT_SCAN_MAX: usize = 1 << 20;
/// Past the `STDOUT_CAPTURE_CAP` head, at least this much of the stream's end
/// is kept, because the result is the last line: keeping only the head would
/// lose the answer of any run that outgrew the cap. Metrics are read from
/// every line as it arrives, so the cut never drops a result event's usage.
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

    fn skipped(&mut self) {
        if let Ok(mut metrics) = self.metrics.lock() {
            metrics.skipped();
        }
    }

    fn eof(&mut self) {
        if let Ok(mut metrics) = self.metrics.lock() {
            metrics.eof();
        }
    }
}

/// How long `hrafnar` lets a claude shell command run in the foreground: 20
/// minutes, the allowance grok gets (`GROK_COMMAND_WAIT_SECS`) for the same
/// reason - long enough for a full gate, short enough that a wedged command
/// cannot hold a run for hours. Set as both `BASH_DEFAULT_TIMEOUT_MS` and
/// `BASH_MAX_TIMEOUT_MS`; see `forced_env` for why the default is the one that
/// matters.
const CLAUDE_COMMAND_WAIT_SECS: u64 = 20 * 60;

/// The flags maki accepts for Claude Code compatibility and then ignores with
/// a warning (`Cli::warn_ignored_flags` in maki's `src/cli.rs`). Passing one
/// would silently drop the setting it names, so `launch_args` must never
/// emit them; the unit test holds it to that. Re-check against maki's list
/// after an upgrade.
#[cfg(test)]
const MAKI_IGNORED_FLAGS: [&str; 15] = [
    "--fallback-model",
    "--settings",
    "--setting-sources",
    "--add-dir",
    "--strict-mcp-config",
    "--include-hook-events",
    "--mcp-config",
    "--tools",
    "--betas",
    "--max-thinking-tokens",
    "--effort",
    "--json-schema",
    "--max-budget-usd",
    "--thinking",
    "--thinking-display",
];

/// The argv for one launch, after the harness command's own leading args.
fn launch_args(dialect: Dialect, session: &SessionStart, selection: &ModelSpec) -> Vec<String> {
    let id = session
        .id()
        .expect("validated caller-minted identity")
        .as_str()
        .to_string();
    // A fresh run writes the session under the id `hrafnar` minted; a resume
    // names the one it continues. Both harnesses spell these the same way.
    // maki parses a hyphenated UUID or its own base58 and echoes the caller's
    // string verbatim, so the minted UUID serves it too; a resume id stays
    // opaque, so a session maki named itself can be resumed.
    let mut args: Vec<String> = if session.is_fresh() {
        vec!["--session-id".to_string(), id]
    } else {
        vec!["--resume".to_string(), id]
    };
    // The stream ends in one result object per run, whatever the outcome,
    // carrying `is_error` - so the turn classifies itself, as grok's does, and a
    // failed run still names its session. Text output gives neither: only a
    // non-zero exit, and no session id to keep.
    //
    // `stream-json` rather than `json` (which prints only that object at the
    // end) for its first event: an `init`, within half a second of launch,
    // that makes the harness safe to signal (see the wait below).
    let flags: &[&str] = match dialect {
        // `--verbose` is required with stream-json under `--print`. When claude
        // wakes the model for a background subagent's result, the stream
        // carries a result per turn and the last is the answer - moot while
        // background tasks are disabled (`forced_env`). `dontAsk` uses the
        // pre-approved permissions and rejects any interactive prompt, which a
        // headless run could never answer.
        Dialect::Claude => &[
            "--print",
            "--permission-mode",
            "dontAsk",
            "--output-format",
            "stream-json",
            "--verbose",
        ],
        // maki needs no `--verbose` for stream-json, and takes no permission
        // mode outside its SDK mode (it refuses `--permission-mode` under
        // `-p`): an unattended `-p` run denies whatever would prompt,
        // which is claude's `dontAsk`. Never `--trust`: `hrafnar` grants no
        // folder trust, so an untrusted project's `.maki` config can tighten
        // maki's permissions but not loosen them (trust the operator gave
        // maki itself still applies: a saved trust answer, the operator's
        // `trust.paths`, and then a trusted project's allow rules). maki's
        // built-in rules allow file edits inside the working directory, so a
        // maki with no permissions file can edit the project.
        // Background tasks are already off under `-p`. maki reads the prompt
        // from stdin in full when no prompt argument is given.
        Dialect::Maki => &["-p", "--output-format", "stream-json"],
    };
    args.extend(flags.iter().map(ToString::to_string));
    // The profile's `model` and `effort`. For maki the model is its own
    // `provider/model-id`, provider meaning the model vendor (OpenRouter's
    // `openrouter/free` router is `openrouter/openrouter/free`), and effort
    // is refused at config parsing because maki ignores `--effort`. Without a
    // model maki takes the last one it used, then its configured default.
    args.extend_from_slice(selection.args());
    args
}

/// Environment the adapter sets before the profile's, so a profile can
/// restate any of it.
fn forced_env(dialect: Dialect) -> Vec<(&'static str, String)> {
    match dialect {
        // Keep the model's shell commands in the foreground, where their output
        // reaches it. `--print` does not wait for a background Bash command when
        // the model ends its turn: it *kills* it and exits 0. Probed on Claude
        // Code 2.1.284, asked only to run a 60s command in the background and
        // report its output, the model replied "I'll report exactly what it
        // prints when it finishes", ended its turn, and the command was killed
        // seconds later - a clean-looking run with no report, the loss grok's
        // wake loop exists for. Claude's switch removes the `run_in_background`
        // parameter altogether, so the same prompt ran the command in the
        // foreground and reported its output, and no wake loop is needed. A
        // background *subagent* is waited for even without the switch, its
        // result handed back in a second turn.
        //
        // The cost of the switch is Bash's 2-minute default timeout, which
        // kills a longer foreground command outright. The model can pass its
        // own `timeout`, but under `dontAsk` a permission allowlist that names
        // commands (like `Bash(python3 *)`) denies any call carrying one -
        // observed - so the default is the limit that actually applies, and it
        // is raised to `CLAUDE_COMMAND_WAIT_SECS`: a 150s command then ran to
        // completion. The maximum is raised with it, for a model that is
        // allowed to pass a timeout.
        Dialect::Claude => {
            let wait_ms = (CLAUDE_COMMAND_WAIT_SECS * 1000).to_string();
            vec![
                ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1".to_string()),
                ("BASH_DEFAULT_TIMEOUT_MS", wait_ms.clone()),
                ("BASH_MAX_TIMEOUT_MS", wait_ms),
            ]
        }
        // maki runs on the operator's own config, as claude does on its
        // settings: its bash timeout is a config option (`[plugins.bash]`),
        // not an environment variable, and is the operator's to set. Its XDG
        // directories are not isolated per run, because sessions live under
        // the state directory (isolation breaks resume) and isolating only
        // the config would hide the operator's providers and permissions.
        Dialect::Maki => Vec::new(),
    }
}

pub(super) async fn run_claude_compat(
    dialect: Dialect,
    launch: &super::LaunchSpec,
    launched: Option<LaunchSignal>,
    runtime: &HarnessRuntime,
) -> Result<RunOutput> {
    let harness = dialect.harness();
    let session = &launch.start;
    let selection = &launch.settings.selection;
    // The token goes in last, over the profile's env: it exists only because
    // the child's own credential was already found wanting.
    let with_token = launch.settings.claude_token.as_ref().map(|token| {
        let mut env = launch.settings.env.clone().unwrap_or_default();
        env.insert(
            super::claude_token::TOKEN_VAR.to_string(),
            token.value().to_string(),
        );
        env
    });
    let env = with_token.as_ref().or(launch.settings.env.as_ref());
    let prompt = launch.prompt.as_str();
    let project_root = launch.root.as_path();

    // The invocation owner supplies a fresh UUID; runners never mint IDs.
    // Entry validation also protects direct runner callers.
    selection.validate(harness)?;
    session.validate(harness)?;
    let args = launch_args(dialect, session, selection);

    // Launched under a keeper (`contained::spawn`), which gives the harness
    // its own process group and tears down every shell it started when the
    // run ends.
    let spec = ProcessSpec::new(
        runtime.commands.get(harness),
        args,
        project_root,
        &forced_env(dialect),
        env,
        &[],
        Stdin::Pipe(prompt.as_bytes().to_vec()),
    );
    // The lifecycle clears a stale interrupt request on a resume (one would
    // mark a failed resume as interrupted, hiding claude's stated reason), and
    // refuses to launch once `hrafnar` was told to stop: a launch then would
    // only be killed before its first event - and, a stopped run being
    // recorded as interrupted, file a row and print a resume command for a
    // session claude may never have written.
    //
    // The session id is known before the run starts - generated for a fresh
    // run, passed in for a resume - so the in-flight marker goes up before the
    // spawn. That ordering is load-bearing: `hrafnar message` releases its
    // session launch lock when the launch signal below fires, and a second
    // message must then find this marker, or it launches beside this run
    // instead of interrupting it. Removed on any early return below.
    //
    // The keeper's pid is offered on the marker for `hrafnar interrupt` only
    // once the harness has printed its `init` event (`lease.ready()` in the
    // reader). A SIGINT before claude installs its handler takes the default
    // action and kills it with nothing printed; after, it ends the turn with a
    // result object. maki installs its handler after reading the prompt and
    // before printing `init` (`print::run`), so the same rule holds for it.
    let owner = Lease::known(
        session.id().expect("validated caller-minted identity"),
        harness,
        project_root,
        &runtime.data,
        &runtime.run_id,
        runtime.reservation.clone(),
        runtime.predecessor.as_ref(),
    );
    let lease = Lease::clone(&owner);
    let (mut run, pipes) = Run::spawn(harness, &spec, session, LeaseUse::Owned(owner), runtime)?;

    let stdout_shared = new_shared_buf();
    let capture = Capture::head_tail(
        std::sync::Arc::clone(&stdout_shared),
        STDOUT_CAPTURE_CAP,
        CLAUDE_TAIL_KEEP,
    );
    let claude_metrics = std::sync::Arc::new(std::sync::Mutex::new(ClaudeMetrics::new(dialect)));
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

    // Wait for the harness, but end the turn if `hrafnar` is signalled to
    // stop. Probed on Claude Code 2.1.284 mid-command: a SIGINT to claude ends
    // the turn at once, kills the running command, and still prints a result
    // object (`is_error: true`, `terminal_reason: "aborted_tools"`) naming the
    // session, which then resumes normally. A SIGTERM also leaves a resumable
    // session but prints nothing, so there would be no result to record; the
    // stop path therefore always forwards SIGINT. maki answers SIGINT and
    // SIGTERM alike with a `cancelled` result (exit 130 or 143). Before the
    // first event the harness may not have a handler yet, so then the group
    // is killed instead, as codex's is (`Run::wait`'s stop arm).
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
        &format!("{harness} stdout"),
    )
    .await;
    let stdout_buf = capture.snapshot();
    let stderr_buf = collect_reader(
        stderr_task,
        &stderr_shared,
        runtime.drain_grace,
        &format!("{harness} stderr"),
    )
    .await;
    let write_res = ended.stdin;
    let mut stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
    // Everything below prints or records text derived from these. A claude, or
    // a wrapper around it, that echoes its environment in an error must not
    // carry the injected token into the answer, the digest or the audit log.
    if let Some(token) = &launch.settings.claude_token {
        stdout = stdout.replace(token.value(), "<redacted claude token>");
        stderr = stderr.replace(token.value(), "<redacted claude token>");
    }

    // The echoed ID wins on a fresh run in case claude reassigned the requested
    // UUID; on resume, retain the ID the caller asked us to continue.
    let keep_id = |echoed: Option<String>| session.result_id(echoed);
    let metrics = claude_metrics
        .lock()
        .map(|metrics| metrics.clone())
        .unwrap_or_default();
    let served = metrics.served();
    let auth_failure = metrics.auth_failure();
    let turns = metrics.turns;
    let usage = metrics.usage;
    let denials = match &launch.settings.claude_token {
        // The descriptions quote tool input from the stream, which was read
        // before it was scrubbed.
        Some(token) => Denials {
            shown: metrics
                .denials
                .shown
                .iter()
                .map(|d| d.replace(token.value(), "<redacted claude token>"))
                .collect(),
            ..metrics.denials
        },
        None => metrics.denials,
    };
    let with_usage = |digest: Digest| Digest {
        turns,
        usage: usage.clone(),
        denials: denials.clone(),
        ..digest
    };
    // An abnormal end always keeps its observed status; a clean answer with a
    // clean exit records no digest, as for grok - unless the permission gate
    // refused a call, which the orchestrator must see even beside an answer.
    let forensics = |outcome: &Outcome| {
        if outcome.is_clean() {
            match answered_status_digest(status) {
                Some(digest) => Some(with_usage(digest)),
                None if !denials.is_empty() => Some(with_usage(status_digest(status))),
                None => None,
            }
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
    // Whether the harness printed its first event, which carries the session
    // id. Past that point the session may exist, so a run that then ends
    // without a result keeps its id: a session that turns out not to exist
    // costs one resume that fails cleanly, while a dropped id loses the turn
    // most worth resuming. For claude the transcript is on disk by then. maki
    // saves only once the session holds a message, which is after it connects
    // to the model provider, so a maki killed in between leaves an id with no
    // session behind it; no stream event tells the two apart, and maki
    // refuses a resume of such an id cleanly ("not found", exit 1).
    let started = lease.is_ready();

    let (text, outcome, session_id) = match interpret_claude_output(dialect, &stdout, &stderr) {
        Ok(ClaudeOutcome::Answered { text, session_id }) => {
            (text, Outcome::answered(Answer::Captured), session_id)
        }
        Ok(ClaudeOutcome::NoAnswer {
            stated,
            unverified,
            text,
            session_id,
        }) => {
            let mut outcome = Outcome::no_answer();
            if let Some(reason) = stated {
                outcome = outcome.stated(reason);
            }
            if let Some(reason) = unverified {
                outcome = outcome.unverified(reason);
            }
            (text, outcome, session_id)
        }
        // No result, but the harness had started: interrupted before it
        // could answer for itself, or died (killed from outside, crashed)
        // after starting. Dropping its id - as an `Err` would, `invoke`
        // setting none - makes the one turn most worth resuming unresumable.
        // A death is `Ok` with no stated reason, as for codex, because the
        // harness stated none. An interrupted one shows the interrupt text
        // instead of the error.
        Err(e) if started => (
            if interrupt_requested {
                String::new()
            } else {
                format!("({harness} ended without a result: {e})")
            },
            Outcome::no_answer(),
            None,
        ),
        // Stopped before the first event: the group was killed before the
        // harness could have written anything, so there is no session to
        // name. Recording one would print a resume command that fails.
        Err(_) if stopped_by_signal => {
            anyhow::bail!(
                "not started: hrafnar was signalled to stop before {harness}'s first event"
            );
        }
        // No result object and claude never started: the prompt may never
        // have arrived, which is the more useful thing to say when it didn't.
        Err(e) => match write_res {
            Err(write_err) => return Err(write_err.context(e.to_string())),
            Ok(()) => return Err(e),
        },
    };
    // A failure claude reported, not a run that ended without saying why.
    let auth = if outcome.stated.is_some() && !outcome.has_answer() {
        auth_failure
    } else {
        None
    };
    let outcome = settle(outcome).auth(auth);
    let text = if outcome.interrupted() && text.trim().is_empty() {
        outcome.placeholder(harness)
    } else {
        text
    };
    // And again on what was parsed: the raw replacement above misses a token
    // that a JSON escape spelled differently, and metrics read the stream
    // before it was scrubbed (a model name is printed and recorded).
    const REDACTED: &str = "<redacted claude token>";
    // Command lines in the teardown evidence too: a hook or wrapper can pass
    // the token to a process as an argument.
    let (text, outcome, served, session_id, leftovers) = match &launch.settings.claude_token {
        Some(token) => {
            let scrub = |s: String| s.replace(token.value(), REDACTED);
            let mut leftovers = leftovers;
            for proc in leftovers
                .killed
                .iter_mut()
                .chain(leftovers.survivors.iter_mut())
            {
                proc.cmdline = scrub(std::mem::take(&mut proc.cmdline));
            }
            (
                scrub(text),
                Outcome {
                    stated: outcome.stated.map(scrub),
                    unverified: outcome.unverified.map(scrub),
                    ..outcome
                },
                served.scrubbed(token.value(), REDACTED),
                session_id.map(scrub),
                leftovers,
            )
        }
        None => (text, outcome, served, session_id, leftovers),
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
    summarize_output(Dialect::Claude, stdout)
}

#[cfg(test)]
fn summarize_output(dialect: Dialect, stdout: &str) -> ClaudeMetrics {
    let mut metrics = ClaudeMetrics::new(dialect);
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

/// Neither dialect has hrafnar-managed permission files to prepare: each runs
/// on its own permission configuration.
pub(super) fn prepare(_launch: &super::LaunchSpec) {}
