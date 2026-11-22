//! The grok adapter: one headless JSON result per turn, the prompt given as a
//! `--prompt-file` since grok takes none on stdin. `run_grok` repeats turns
//! until grok's own update log shows no background result the model missed,
//! resuming the session with those results in the prompt (the wake loop). A
//! `workspace-write` run needs a sandbox profile and folder trust set up in
//! grok's state directory, which `state` owns. Grok cannot be interrupted.
//! The mechanism and its evidence are in `reference/grok.md`.

use super::*;
use std::path::{Path, PathBuf};

mod state;

pub(super) use state::{ProfileName, RootSet};

/// Grok's headless result object (`--output-format json`). One JSON object on
/// stdout per run, whatever the outcome; the fields we don't use (usage,
/// requestId, thought) are ignored rather than modelled. The served model and
/// cost are read separately, by `result_served`.
#[derive(serde::Deserialize)]
struct GrokResult {
    #[serde(default)]
    text: String,
    #[serde(rename = "stopReason", default)]
    stop_reason: Option<String>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    /// Grok also prints failures that happen *before* a turn as a typed line on
    /// stdout (`{"type":"error","message":...}`). Every field above is optional,
    /// so such a line parses cleanly as an all-default result - these two carry
    /// the reason that would otherwise be silently discarded.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Not read for its value - only as evidence that grok got as far as
    /// running a turn. See `ran_a_turn`.
    #[serde(rename = "numTurns", alias = "num_turns", default)]
    num_turns: Option<u32>,
}

impl GrokResult {
    /// Whether this object is evidence that a turn actually ran.
    ///
    /// Every field is optional, so *any* JSON object deserializes into an
    /// all-default `GrokResult`. Treating that as "a turn ran but did not
    /// answer" infers the fact from successful parsing rather than from
    /// evidence, and the inference is load-bearing: `NoAnswer` returns `Ok`,
    /// which tells `review resume` the prompt cache was warmed and refreshes the
    /// staleness clock. An unrecognised pre-turn failure would therefore mark a
    /// cold session warm. Requiring one of grok's own result fields keeps that
    /// claim tied to something grok actually said.
    fn ran_a_turn(&self) -> bool {
        self.stop_reason.is_some() || self.session_id.is_some() || self.num_turns.is_some()
    }
}

/// How long `review` lets a grok shell command run: 20 minutes. Three grok
/// limits are set from it:
/// - the auto-background budget (`GROK_FOREGROUND_BLOCK_BUDGET_MS`, default
///   15s): a command holds the foreground for `min(model's timeout, this)` -
///   the model's timeout may be up to 10h, since production grok always sends
///   `max_timeout_secs` = 10h (`tools/config.rs` `PRODUCTION_MAX_TIMEOUT_SECS`),
///   and is 120s when the model sets none;
/// - the kill cap for a command that *cannot* be backgrounded
///   (`GROK_MAX_FOREGROUND_BLOCK_MS`, default 5 minutes) - not the normal path,
///   set so that path gets the same allowance rather than a shorter one;
/// - how long headless grok waits for a backgrounded command after the model's
///   turn ends before killing it (`--background-wait-timeout`, default 600s).
///
/// Long enough for a full gate; short enough that a wedged command cannot hold a
/// run for hours.
const GROK_COMMAND_WAIT_SECS: u64 = 20 * 60;

/// The one `stopReason` that means `text` is an answer. Anything else
/// (`cancelled`, `max_tokens`, ...) means `text` is whatever the model happened
/// to have said when it was cut off - interim commentary, not a result.
const GROK_OK_STOP: &str = "end_turn";

/// How many times `run_grok` will wake a model for a background result it did
/// not read. A model that reads the result and finishes needs one; the cap only
/// stops a model that keeps backgrounding work from looping forever, and
/// hitting it fails the run rather than passing it off as done.
const GROK_MAX_WAKES: u32 = 10;

/// Run grok, and keep waking it until it has read every background result.
///
/// Headless grok ends when the model ends its turn, even if a command the model
/// started is still running in the background: it waits for the command to
/// finish, queues a wake-up message for the model, and exits without running
/// that turn (`headless.rs` has no wake handling). Whether the model waited for
/// its own command first is its choice, and a wrong choice is silent - a run
/// that looks finished, with the build's result never read and no report. So
/// the guarantee is enforced here instead of hoped for: after every turn,
/// grok's own update stream is checked for results the model never saw
/// (`state::unread_background_results`), and if there are any the session
/// is resumed, same sandbox and model, with a prompt that **contains** them
/// (`state::wake_prompt`: each command's exit status and output tail, or a
/// plain "did NOT finish"). Carrying the result rather than pointing at it is
/// what makes delivery independent of the model: a prompt that only told it to
/// go and read the output could be ignored, and the next turn boundary would
/// then look clean. The run returns once a turn ends with nothing unread.
///
/// It fails rather than returns clean whenever the results cannot be vouched
/// for: grok's home or update log cannot be read or parsed (fail closed), or
/// `GROK_MAX_WAKES` is reached (each wake delivers what it names, so the cap
/// only stops a model that keeps starting new background work). It does not
/// wake after a stated failure - grok said why the turn ended, and resending
/// buys the same answer (`Outcome::stated_failure`); that outcome already
/// fails the run. A wake turn that fails to launch keeps the first turn's output and
/// session id rather than dropping both.
pub(super) async fn run_grok(
    launch: &super::LaunchSpec,
    sandbox: Option<&str>,
    prepared: &Prepared,
    launched: Option<LaunchSignal>,
    runtime: &HarnessRuntime,
) -> Result<RunOutput> {
    let session = &launch.start;
    let selection = &launch.settings.selection;
    let project_root = launch.root.as_path();

    selection.validate(Harness::Grok)?;
    session.validate(Harness::Grok)?;
    let session_id = session.id().expect("validated grok identity").as_str();
    let data = &runtime.data;
    // In flight for the whole run, wake turns included. Never offered a pid:
    // grok has no clean way to end a turn, so `review interrupt` refuses it.
    // What the marker buys is `review message` seeing the turn in flight and
    // refusing, as its help says, instead of launching a second turn on the
    // session beside this one - and `review sessions` showing it. A resume
    // marks before launch, including fresh caller-minted identities. If the
    // harness echoes a different identity, the lease holds a marker for both
    // through the wakes, each kept current with the turn's keeper. One lease
    // for the whole run, closed when this function returns however it does.
    let lease = Lease::known(
        session.id().expect("validated grok identity"),
        Harness::Grok,
        project_root,
        data,
        &runtime.run_id,
        runtime.reservation.clone(),
        runtime.predecessor.as_ref(),
    );
    let first = run_grok_turn(
        launch,
        sandbox,
        prepared,
        prepared.trust_grant.is_some(),
        launched,
        runtime,
        &lease,
    )
    .await?;
    // A fresh run reports its new id (grok's echo, else the one we minted); a
    // resume was given it. So this is always known.
    let sid = first
        .out
        .session_id
        .clone()
        .unwrap_or_else(|| session_id.to_string());
    if sid != session_id {
        lease.alias(&sid);
    }
    let grok = &prepared.grok;
    let mut run = first;
    let mut wakes = 0;
    loop {
        // Fail closed: a run whose results cannot be checked is not a run
        // whose results were seen.
        let pending = match grok.wake_check(&sid) {
            state::WakeCheck::Clean => break,
            state::WakeCheck::Unread(pending) => pending,
            state::WakeCheck::Unverifiable(reason) => {
                let reason = format!("could not verify grok's background results: {reason}");
                eprintln!("grok: {reason}");
                run.out.digest = Some(wake_failure(&mut run.out, reason));
                break;
            }
        };
        // A failed turn is not fixed by waking, whether grok stated a reason or
        // review found its stop reason missing. The outcome already fails it.
        if run.out.outcome.explained_failure() {
            break;
        }
        // A signal can arrive after this turn but before we wake it. Do not
        // start another turn just to have the keeper shut it down. Preserve
        // the answer, but fail the run because its background results stayed
        // unread. This notification snapshot selects the wake result policy;
        // it does not authorize a launch. Run::spawn still uses the lifecycle's
        // admission gate if stop arrives after this check.
        if runtime.lifecycle.stop_requested() {
            let reason = "grok wake-up not launched: review was signalled to stop".to_string();
            eprintln!("grok: {reason}");
            run.out.digest = Some(wake_failure(&mut run.out, reason));
            break;
        }
        let names: Vec<String> = pending.iter().map(state::Unread::id).collect();
        if wakes == GROK_MAX_WAKES {
            let reason = format!(
                "grok still had unread background results after {GROK_MAX_WAKES} wake-ups: {}",
                names.join(", ")
            );
            eprintln!("grok: {reason} - giving up on session {sid}");
            run.out.digest = Some(wake_failure(&mut run.out, reason));
            break;
        }
        wakes += 1;
        eprintln!(
            "grok: session {sid} ended its turn with unread background results ({}) - \
             resuming it with them (wake {wakes}/{GROK_MAX_WAKES})",
            names.join(", ")
        );
        // Same sandbox (grok refuses a different one on resume) and model; no
        // `--trust` (granted on the first turn if it was needed); no launch
        // signal (the lock was released when the first turn spawned).
        let prompt = state::wake_prompt(&pending);
        let wake = match SessionId::parse(&sid) {
            Ok(id) => SessionStart::Resume(id),
            Err(e) => {
                run.out.digest = Some(wake_failure(
                    &mut run.out,
                    format!("invalid wake session id: {e}"),
                ));
                break;
            }
        };
        let wake_launch = super::LaunchSpec {
            start: wake,
            prompt,
            ..launch.clone()
        };
        let next = match run_grok_turn(
            &wake_launch,
            sandbox,
            prepared,
            false,
            None,
            runtime,
            &lease,
        )
        .await
        {
            Ok(next) => next,
            Err(e) => {
                // Keep what the run already has: `invoke` drops the session id
                // on `Err`, which would cost the sidecar row and any resume.
                let reason = format!("wake-up turn for session {sid} failed: {e:#}");
                eprintln!("grok: {reason}");
                run.out.digest = Some(wake_failure(&mut run.out, reason));
                break;
            }
        };
        run.absorb(next);
    }
    let mut out = run.finish();
    if wakes > 0 {
        out.text = format!(
            "(grok woken {wakes} time(s) to read background results it had not waited for)\n{}",
            out.text
        );
    }
    Ok(out)
}

/// Mark a verification failure without rewriting the harness's answer,
/// stated reason, or observed process status, and return the digest the run
/// keeps. No digest from `run_grok_turn` means it captured an answer and
/// observed a successful exit.
fn wake_failure(run: &mut RunOutput, reason: String) -> Digest {
    run.outcome.unverified = Some(reason);
    run.digest
        .take()
        .unwrap_or_else(|| status_digest(crate::process::Exit::Code(0)))
}

/// One grok turn's output with its counts, which a clean answer carries no
/// digest for, and the running total over a run's wake turns.
struct GrokTurn {
    out: RunOutput,
    turns: u32,
    usage: Usage,
}

impl GrokTurn {
    /// Fold in the next wake turn of the *same* run. Sequential-continuation
    /// rules, which is why this is grok's and not a general `RunOutput`
    /// operation (an automatic recovery has other identity and
    /// outcome rules): the wake's text, outcome and digest replace the
    /// earlier turn's whole - an outcome is never assembled from two turns -
    /// the session id stays the first turn's (a wake resumes that session),
    /// counts and usage sum, served models union, costs sum where known, and
    /// every turn's leftovers stay on the run.
    fn absorb(&mut self, next: GrokTurn) {
        self.turns += next.turns;
        self.usage += &next.usage;
        let mut next_out = next.out;
        let mut served = std::mem::take(&mut self.out.served);
        served.merge(std::mem::take(&mut next_out.served));
        let mut leftovers = std::mem::take(&mut self.out.leftovers);
        leftovers.merge(std::mem::take(&mut next_out.leftovers));
        self.out = RunOutput {
            session_id: self.out.session_id.take(),
            served,
            leftovers,
            ..next_out
        };
    }

    /// The run's output. Whatever digest it ends with, including one a wake
    /// failure created, describes the whole run, not its last grok process.
    fn finish(mut self) -> RunOutput {
        if let Some(d) = self.out.digest.as_mut() {
            d.turns = self.turns;
            d.usage = self.usage;
        }
        self.out
    }
}

/// One grok process: a single headless turn.
async fn run_grok_turn(
    launch: &super::LaunchSpec,
    // Prepared selector: never reconstruct this from requested permissions,
    // including on a wake whose request still describes a fresh workspace.
    sandbox: Option<&str>,
    prepared: &Prepared,
    trust: bool,
    launched: Option<LaunchSignal>,
    runtime: &HarnessRuntime,
    lease: &Lease,
) -> Result<GrokTurn> {
    let grok = &prepared.grok;
    let session = &launch.start;
    let selection = &launch.settings.selection;
    let env = launch.settings.env.as_ref();
    let prompt = launch.prompt.as_str();
    let project_root = launch.root.as_path();
    selection.validate(Harness::Grok)?;
    session.validate(Harness::Grok)?;
    // Grok takes no prompt on stdin - `-p` requires a value and `-p -` is read
    // literally as a one-character prompt. `--prompt-file` is the equivalent
    // escape from shell argument length limits, so the prompt goes via a temp
    // file instead of a pipe.
    let prompt_path = new_temp_file("grok-prompt")?;
    std::fs::write(&prompt_path, prompt)
        .with_context(|| format!("failed to write grok prompt file {prompt_path}"))?;
    let _prompt_file = RemoveOnDrop(prompt_path.clone());

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
    args.push("--prompt-file".to_string());
    args.push(prompt_path.clone());
    args.push("--output-format".to_string());
    args.push("json".to_string());
    args.push("--permission-mode".to_string());
    args.push("dontAsk".to_string());
    // Unlike claude, grok has a real filesystem sandbox on the same axis as
    // codex's, so the profile field maps honestly and the default still holds:
    // a run with no profile cannot modify files.
    //
    // Except on a resume with no recorded level (a session whose sidecar row
    // predates sandbox recording): grok fixes a session's profile
    // for its lifetime and refuses an explicit `--sandbox` that differs from the
    // saved one (`resolve_startup_sandbox`: explicit + saved, different =>
    // `Conflict`), while no flag applies the saved one. Forcing `read-only` there
    // made every such write session unresumable. Omitting the flag defers to
    // what the session already had - no trust or root setup happens, because
    // there is nothing recorded to say the session writes.
    if let Some(level) = sandbox {
        args.push("--sandbox".to_string());
        args.push(level.to_string());
    }
    // Grok's own (hidden, headless-honoured) grant: records the project in
    // `trusted_folders.toml` under grok's key and lock before the turn starts.
    // Without it every edit in an untrusted folder cancels the turn under
    // `dontAsk`. Passed only when the project has no decision yet, and
    // announced by adapter preparation, which has also refused the cases where the grant
    // would be wrong (see `prepare_grok_write`).
    if trust {
        args.push("--trust".to_string());
    }
    args.extend_from_slice(selection.args());
    // When the model ends its turn with a command still in the background,
    // headless grok waits for it - by default only 600s, then kills it
    // (`reap_pending_background_tasks`). Raised to the same wait as a foreground
    // command, so a build the model walked away from still finishes and leaves
    // its output for the wake-up turn `run_grok` then gives the model. Hidden
    // but headless-honoured (`cli.rs` `background_wait_timeout_secs`).
    args.push("--background-wait-timeout".to_string());
    args.push(GROK_COMMAND_WAIT_SECS.to_string());

    let mut cmd = runtime.commands.get(Harness::Grok).build();
    cmd.args(&args).current_dir(project_root);
    // Keep shell commands in the foreground until they finish. Grok moves any
    // command that blocks longer than 15s to the background, and headless `-p`
    // then only waits for it to *finish* before exiting - it never gives the
    // model another turn to read the result (`headless.rs`: exits once
    // `pending_bg` is empty). So every build over 15s ended a fixer's turn
    // without a report: the second wave lost two that way, and a live probe
    // watched `brokkr test` pass 13s after grok had already stopped listening.
    // The switch itself (`toolset.bash.auto_background_on_timeout`) is
    // config.toml-only and not on the `GROK_CONFIG` allowlist, and turning it
    // off would *kill* commands at a 5-minute cap instead, so the budgets are
    // raised per run through grok's own env overrides: the auto-background
    // budget (`GROK_FOREGROUND_BLOCK_BUDGET_MS`, terminal.rs) and the
    // non-backgroundable cap (`GROK_MAX_FOREGROUND_BLOCK_MS`, bash/mod.rs), both
    // to `GROK_COMMAND_WAIT_SECS`. A command then blocks until it exits or hits
    // the timeout the model asked for (120s when it asks for none). That keeps
    // most commands in the foreground but cannot guarantee it - the model
    // chooses the timeout - which is why `run_grok` also wakes the model for any
    // result it missed. Set before the profile env so a profile can restate
    // either.
    let wait_ms = (GROK_COMMAND_WAIT_SECS * 1000).to_string();
    let spec = ProcessSpec::new(
        runtime.commands.get(Harness::Grok),
        args,
        project_root,
        &[
            ("GROK_FOREGROUND_BLOCK_BUDGET_MS", wait_ms.clone()),
            ("GROK_MAX_FOREGROUND_BLOCK_MS", wait_ms),
        ],
        env,
        &[],
        Stdin::Null,
    );
    // Re-checked immediately before spawn. `check_trust` in preparation and grok's
    // own grant are two separate reads of the store, and grok's grant overwrites
    // a `trusted = false` - so a "no" recorded between them (by the operator, or
    // another grok) would be overturned. This cannot close that race from
    // outside grok, which offers no "grant unless declined" operation; it
    // shrinks the window from the whole setup to the time grok takes to start.
    if trust {
        prepared.check_grant_target(project_root)?;
    }
    // The lifecycle checks the stop channel immediately before launch, and
    // records this turn's keeper on the run's lease (every marker it holds)
    // before grok starts. Grok is not interruptible, so the run has no stop
    // arm: the signal supervisor terminates its keeper.
    let (mut run, pipes) = Run::spawn(
        Harness::Grok,
        &spec,
        session,
        LeaseUse::Shared(lease.clone()),
        runtime,
    )?;
    // Read while the run goes, not after: the keeper is reaped only once the
    // teardown is done, and a reader started late could leave grok blocked on
    // a full pipe.
    let stdout_shared = new_shared_buf();
    let stderr_shared = new_shared_buf();
    let stdout_task = tokio::spawn(read_capped(
        pipes.stdout,
        STDOUT_CAPTURE_CAP,
        std::sync::Arc::clone(&stdout_shared),
    ));
    let stderr_task = tokio::spawn(read_capped(
        pipes.stderr,
        STDERR_CAPTURE_CAP,
        std::sync::Arc::clone(&stderr_shared),
    ));
    run.track(&stdout_task);
    run.track(&stderr_task);
    run.launch(launched).await?;
    run.wait(std::future::pending::<()>()).await;
    let ended = run.finish().await;
    let leftovers = ended.leftovers;
    // `Unknown` when the keeper could not report it; what grok printed still
    // stands, and the ledger fails the command for the unknown teardown.
    let status = ended.status;
    let stdout_buf = collect_reader(
        stdout_task,
        &stdout_shared,
        runtime.drain_grace,
        "grok stdout",
    )
    .await;
    let stderr_buf = collect_reader(
        stderr_task,
        &stderr_shared,
        runtime.drain_grace,
        "grok stderr",
    )
    .await;
    let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
    // A failed `--trust` grant is printed only to stderr, which is otherwise
    // read only when interpreting an error - so it would stay invisible on any
    // turn that happened not to edit, and the next run would be untrusted.
    if trust {
        for line in state::trust_failures(&stderr) {
            eprintln!("warning: grok --trust: {line}");
        }
    }
    // A fresh run reports grok's echoed ID or the UUID we requested, so a
    // future grok that reassigns it cannot orphan the session. A resume reports
    // the requested ID too, so output can name the session while resume
    // recording continues to use that same caller-known ID.
    // Where to look for the event log if grok does not echo an id.
    let lookup_id = fresh_id.clone().unwrap_or_else(|| session_id.to_string());
    let keep_id = |echoed: Option<String>| session.result_id(echoed);
    let served = result_served(&stdout);
    let (turns, usage) = grok_usage(&stdout);
    let with_usage = |digest: Digest| Digest {
        turns,
        usage: usage.clone(),
        ..digest
    };

    let run = match interpret_grok_output(&stdout, &stderr)? {
        GrokOutcome::Answered(answer) => RunOutput {
            text: answer.text,
            outcome: Outcome::answered(Answer::Captured),
            session_id: keep_id(answer.session_id),
            codex_home: None,
            served,
            leftovers,
            // A real answer from a process that nonetheless exited non-zero is
            // still a real answer, but the status is worth surfacing rather
            // than discarding - it is the only sign that something went wrong
            // after the turn finished.
            digest: answered_status_digest(status).map(with_usage),
        },
        GrokOutcome::NoAnswer {
            reason,
            review_error,
            text,
            session_id,
        } => {
            // The result object says only `cancelled`; grok's own event log
            // says why, and folder trust is the cause worth naming outright.
            let cause = {
                let sid = session_id.as_deref().unwrap_or(&lookup_id);
                state::session_events(&grok.home, sid)
                    .as_deref()
                    .and_then(state::cancel_cause)
            };
            let untrusted = grok.untrusted_hint(project_root);
            let reason = explain_grok_no_answer(reason, cause.as_ref(), untrusted.as_deref());
            let outcome = if review_error {
                Outcome::no_answer().unverified(reason)
            } else {
                Outcome::no_answer().stated(reason)
            };
            RunOutput {
                text,
                outcome,
                session_id: keep_id(session_id),
                codex_home: None,
                digest: Some(with_usage(status_digest(status))),
                served,
                leftovers,
            }
        }
    };
    // Turn and token counts go back separately: a clean answer carries no
    // digest, and `run_grok` needs every turn's counts to describe the run.
    Ok(GrokTurn {
        out: run,
        turns,
        usage,
    })
}

/// Extend grok's bare stop reason with what its event log and folder trust
/// say. Pure so the wording is tested without a grok home.
fn explain_grok_no_answer(
    reason: String,
    cause: Option<&state::CancelCause>,
    untrusted: Option<&str>,
) -> String {
    let mut out = reason;
    if let Some(cause) = cause {
        out.push_str(&format!(" - {cause}"));
    }
    if let Some(untrusted) = untrusted {
        out.push_str(&format!(" - {untrusted}"));
    }
    out
}

/// Turn count and token usage from grok's result object, mapped onto the
/// codex-shaped `Usage`: codex's `input_tokens` includes the cached part, so
/// grok's separately-reported cache reads are added back in. Record-only, like
/// `result_served`: a result without these fields records zeros.
fn grok_usage(stdout: &str) -> (u32, Usage) {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct RawUsage {
        input_tokens: u64,
        cache_read_input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    }
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        usage: RawUsage,
        #[serde(rename = "numTurns", alias = "num_turns", default)]
        num_turns: u32,
    }
    let Ok(raw) = serde_json::from_str::<Raw>(stdout.trim()) else {
        return (0, Usage::default());
    };
    let u = raw.usage;
    (
        raw.num_turns,
        // Grok reports no cache writes; nothing is inferred for them.
        Usage::from_parts(
            u.input_tokens,
            0,
            u.cache_read_input_tokens,
            u.output_tokens,
            u.reasoning_tokens,
        ),
    )
}

/// A grok run that produced a real answer.
#[derive(Debug)]
struct GrokAnswer {
    text: String,
    session_id: Option<String>,
}

/// Decide whether a finished grok run produced an answer, from its streams
/// alone.
///
/// Two distinct failures have to stay distinguishable, because the operator's
/// next move differs. A run that never reached a turn at all (unknown model,
/// missing session, auth failure) prints a bare message and no result object -
/// nothing ran, so nothing was spent. A run that reached a turn and was cut off
/// emits a full result object whose `text` is whatever the model had said at
/// that moment: interim commentary, not a result. That second shape is exactly
/// the one that cost so much effort on codex, where it is indistinguishable
/// from a real answer without going to the rollout; grok labels it itself via
/// `stopReason`, so this takes it at face value rather than re-deriving it. The
/// commentary is carried in the error text - it is often the only clue about
/// what the run was doing - but it never becomes the response.
fn interpret_grok_output(stdout: &str, stderr: &str) -> Result<GrokOutcome> {
    let Ok(result) = serde_json::from_str::<GrokResult>(stdout.trim()) else {
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        anyhow::bail!("grok produced no result object: {detail}");
    };

    // A typed error line is well-formed JSON, so it survives the parse above as
    // an empty result. Check it before `stopReason`, or the reason grok gave is
    // replaced by the generic "no stop reason" complaint.
    if result.kind.as_deref() == Some("error") {
        let detail = result.message.as_deref().unwrap_or(stdout.trim());
        anyhow::bail!("grok reported an error: {detail}");
    }

    // Nothing here identifies this as a result object, so it is not evidence a
    // turn ran - treat it like any other pre-turn failure.
    if !result.ran_a_turn() {
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        anyhow::bail!("grok produced no recognisable result: {detail}");
    }

    let stop = result.stop_reason.as_deref().unwrap_or("missing");
    if stop != GROK_OK_STOP {
        return Ok(GrokOutcome::NoAnswer {
            reason: format!("grok ended the turn without an answer (stopReason: {stop})"),
            review_error: result.stop_reason.is_none(),
            // Whatever the model had said when it was cut off. Kept because it
            // is all there is and often the only clue about what the run was
            // doing - the digest, not this text, is what says it is not an
            // answer.
            text: result.text,
            session_id: result.session_id,
        });
    }

    Ok(GrokOutcome::Answered(GrokAnswer {
        text: result.text,
        session_id: result.session_id,
    }))
}

/// What a finished grok run amounts to. An `Err` alongside these two means the
/// run never got as far as a turn at all.
#[derive(Debug)]
enum GrokOutcome {
    Answered(GrokAnswer),
    /// A turn ran and either grok stated a non-answer or omitted its stop reason.
    NoAnswer {
        reason: String,
        /// `true` when review inferred a failure because grok omitted the
        /// field that says how its turn ended.
        review_error: bool,
        text: String,
        session_id: Option<String>,
    },
}

/// How a grok run that may write is launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GrokWrite {
    /// A fresh write run: roots derived, launched under the `review-ws-*`
    /// profile named for them.
    Fresh,
    /// A resume of a session created under a `review-ws-*` profile: the same
    /// profile, with the roots recorded with the session. Grok fixes a
    /// session's sandbox profile for its lifetime and refuses a different
    /// `--sandbox` on resume (`resolve_startup_sandbox` → `Conflict`).
    Resume(ProfileName),
    /// A resume of a session created under grok's built-in `workspace`, before
    /// `review` made profiles. It can only continue as it began: trusted, but
    /// without the extra roots.
    BuiltinWorkspace,
}

/// What `prepare_grok_write` decided.
#[derive(Debug, Default)]
pub(super) struct GrokPrepared {
    /// The `review-ws-*` profile to launch under, when the run uses one.
    pub(super) profile: Option<RootSet>,
    /// The trust key `--trust` will grant, when the project had no decision
    /// yet. `None` when it was already trusted: no flag, no grant, nothing to
    /// announce.
    pub(super) trust_grant: Option<PathBuf>,
}

/// Get grok's own config ready for a write run.
///
/// Trust is granted by grok (`--trust`, grok's own key and lock), and only when
/// the project has no decision yet - a grant is a **permanent**, host-wide
/// decision in `trusted_folders.toml`, and it means more than "may edit": grok's
/// folder trust gates whether the repo's own `.grok` MCP/LSP config may spawn
/// servers (`trust.rs` module docs), so from then on every grok session there,
/// interactive ones included, runs that config without asking. It is therefore
/// announced on the run that makes it, and recorded in the sidecar.
/// `check_trust` refuses the cases where the grant would be wrong.
///
/// Creating a profile is printed, because it edits the operator's file and an
/// edit nobody saw is the ambient widening the rest of this file refuses; an
/// existing profile is silent - it was announced when it was made.
fn prepare_grok_write(
    kind: &GrokWrite,
    project_root: &Path,
    roots: &RootSet,
    grok: &state::Grok,
) -> Result<GrokPrepared> {
    // Validate recorded binding before announcing trust or writing a profile.
    let name = match kind {
        GrokWrite::BuiltinWorkspace => None,
        GrokWrite::Fresh => Some(roots.profile_name()),
        GrokWrite::Resume(name) => {
            // The recorded roots must be the set the session's profile is named
            // for; anything else is a row edited by hand or a record from a
            // different run, and launching would grant a set nobody recorded.
            if roots.profile_name() != *name {
                anyhow::bail!(
                    "the roots recorded with this session do not match its grok \
                     sandbox profile `{name}` - start a fresh run"
                );
            }
            Some(name.clone())
        }
    };
    let mut prepared = GrokPrepared::default();
    let (key, trust) = grok.check_trust(project_root)?;
    if trust == state::Trust::Undecided {
        eprintln!(
            "grok: trusting {} permanently in {} - a write run's edits are cancelled in \
             an untrusted folder; grok will also run this repo's .grok MCP/LSP config \
             without asking from now on",
            key.display(),
            grok.home.join("trusted_folders.toml").display()
        );
        prepared.trust_grant = Some(key);
    }
    let Some(name) = name else {
        return Ok(prepared);
    };
    // Recreated on resume if someone deleted it: the name pins the exact set,
    // so recreating it grants nothing the session did not already have.
    if grok.ensure_profile(roots)? {
        eprintln!(
            "grok: created sandbox profile `{name}` in {} (writable roots: {})",
            grok.home.join("sandbox.toml").display(),
            if roots.paths().is_empty() {
                "none beyond grok's workspace".to_string()
            } else {
                roots.paths().join(", ")
            }
        );
    }
    prepared.profile = Some(roots.clone());
    Ok(prepared)
}

/// Prepared adapter state. The grant remains provisional until the post-run
/// store read; neither preparation nor passing --trust proves persistence.
pub(super) struct Prepared {
    grok: state::Grok,
    trust_grant: Option<PathBuf>,
    verified_grant: Option<String>,
}

pub(super) async fn prepare(
    launch: &super::LaunchSpec,
    plan: &mut super::permissions::PermissionPlan,
) -> Result<Prepared> {
    let grok = state::Grok::resolve(launch.settings.env.as_ref())?;
    let mut prepared = Prepared {
        grok,
        trust_grant: None,
        verified_grant: None,
    };
    if let Some(kind) = plan.permissions.write_setup() {
        let root = launch.root.clone();
        let roots = RootSet::new(&plan.paths);
        let state = prepared.grok.clone();
        let write =
            tokio::task::spawn_blocking(move || prepare_grok_write(&kind, &root, &roots, &state))
                .await
                .map_err(|e| anyhow::anyhow!("grok setup task failed: {e}"))??;
        if let Some(name) = write.profile {
            plan.grok_profile(name);
        }
        prepared.trust_grant = write.trust_grant;
    }
    Ok(prepared)
}

impl Prepared {
    pub(super) async fn run(
        &mut self,
        launch: &super::LaunchSpec,
        sandbox: Option<&str>,
        launched: Option<LaunchSignal>,
        runtime: &HarnessRuntime,
    ) -> Result<RunOutput> {
        let result = run_grok(launch, sandbox, self, launched, runtime).await;
        self.verify_trust(&launch.root);
        result
    }

    /// Only a turn passing --trust needs this extra admission check. Turns
    /// without a grant leave trust enforcement to Grok itself.
    fn check_grant_target(&self, root: &Path) -> Result<()> {
        let (current, _) = self.grok.check_trust(root)?;
        if self.trust_grant.as_deref() != Some(current.as_path()) {
            anyhow::bail!(
                "grok trust key changed before granting trust - refusing launch for {}",
                root.display()
            );
        }
        Ok(())
    }

    fn verify_trust(&mut self, root: &Path) {
        self.verified_grant = None;
        if let Some(key) = &self.trust_grant {
            match self.grok.observe_trust(root) {
                Ok((current, _)) if current != *key => {
                    eprintln!(
                        "warning: grok trust key changed from {} to {} - no durable grant recorded",
                        key.display(),
                        current.display()
                    );
                }
                Ok((_, decision)) => match decision {
                    state::Trust::Trusted => {
                        self.verified_grant = Some(key.to_string_lossy().into_owned());
                    }
                    state::Trust::Declined => {
                        eprintln!(
                            "warning: grok folder {} is explicitly untrusted - no durable grant recorded",
                            key.display()
                        );
                    }
                    state::Trust::Undecided => {
                        eprintln!(
                            "warning: grok --trust did not durably trust {} - the next write run there will try again",
                            key.display()
                        );
                    }
                },
                Err(e) => {
                    eprintln!(
                        "warning: could not verify grok trust for {}: {e:#} - no durable grant recorded",
                        key.display()
                    );
                }
            }
        }
    }

    pub(super) fn record(&self, result: &mut HarnessResult) {
        result.grok_trust = self.verified_grant.clone();
    }
}

#[cfg(test)]
mod tests;
