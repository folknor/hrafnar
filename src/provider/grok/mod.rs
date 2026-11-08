use super::*;

mod state;

pub(super) use state::canonical_roots;

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
/// buys the same answer (`app::stated_failure`); that digest already fails the
/// run. A wake turn that fails to launch keeps the first turn's output and
/// session id rather than dropping both.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_grok(
    session: &SessionStart,
    selection: &super::ModelSpec,
    sandbox: Option<&str>,
    trust: bool,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
    launched: Option<LaunchSignal>,
    runtime: &ProviderRuntime,
) -> Result<RunOutput> {
    selection.validate(Provider::Grok)?;
    session.validate(Provider::Grok)?;
    let session_id = session.id().expect("validated grok identity").as_str();
    let data_root = runtime.data_root.as_deref();
    // In flight for the whole run, wake turns included. Never offered a pid:
    // grok has no clean way to end a turn, so `review interrupt` refuses it.
    // What the marker buys is `review message` seeing the turn in flight and
    // refusing, as its help says, instead of launching a second turn on the
    // session beside this one - and `review sessions` showing it. A resume
    // marks before launch, including fresh caller-minted identities. If the
    // provider echoes a different identity, retain both guards through wakes.
    let project = project_root.to_string_lossy().into_owned();
    let mut _marker = Some(crate::inflight::mark(
        session_id,
        Provider::Grok,
        &project,
        data_root,
    ));
    let (mut first, first_turns, first_usage) = run_grok_turn(
        session,
        selection,
        sandbox,
        trust,
        env,
        prompt,
        project_root,
        launched,
        runtime,
        _marker.as_mut(),
    )
    .await?;
    // A fresh run reports its new id (grok's echo, else the one we minted); a
    // resume was given it. So this is always known.
    let sid = first
        .session_id
        .clone()
        .unwrap_or_else(|| session_id.to_string());
    let _echo_marker = if sid != session_id {
        Some(crate::inflight::mark(
            &sid,
            Provider::Grok,
            &project,
            data_root,
        ))
    } else {
        None
    };
    // Fail closed: without grok's home the results cannot be checked, and an
    // unchecked run must not pass as one whose results were seen.
    let grok = match state::Grok::resolve(env) {
        Ok(g) => g,
        Err(e) => {
            let reason = format!("could not verify grok's background results: {e:#}");
            eprintln!("grok: {reason}");
            let mut digest = wake_failure_digest(reason, first.digest.take());
            digest.turns = first_turns;
            digest.usage = first_usage;
            first.digest = Some(digest);
            return Ok(first);
        }
    };
    let mut out = first;
    let mut cost = out.served.cost_usd;
    let mut turns = first_turns;
    let mut usage = first_usage;
    let mut wakes = 0;
    loop {
        // Fail closed: a run whose results cannot be checked is not a run
        // whose results were seen.
        let pending = match state::session_updates(&grok.home, &sid) {
            None => Err(format!("no update log for grok session {sid}")),
            Some(log) => state::unread_background_results(&log),
        };
        let pending = match pending {
            Ok(p) => p,
            Err(e) => {
                let reason = format!("could not verify grok's background results: {e}");
                eprintln!("grok: {reason}");
                out.digest = Some(wake_failure_digest(reason, out.digest.take()));
                break;
            }
        };
        if pending.is_empty() {
            break;
        }
        // A failed turn is not fixed by waking, whether grok stated a reason or
        // review found its stop reason missing. The digest already fails it.
        if out
            .digest
            .as_ref()
            .is_some_and(|d| (d.turn_error.is_some() || d.review_error.is_some()) && !d.captured)
        {
            break;
        }
        // A signal can arrive after this turn but before we wake it. Do not
        // start another turn just to have the keeper shut it down. Preserve
        // the answer, but fail the run because its background results stayed
        // unread.
        if *runtime.stop.borrow() {
            let reason = "grok wake-up not launched: review was signalled to stop".to_string();
            eprintln!("grok: {reason}");
            out.digest = Some(wake_failure_digest(reason, out.digest.take()));
            break;
        }
        let names: Vec<String> = pending.iter().map(state::Unread::id).collect();
        if wakes == GROK_MAX_WAKES {
            let reason = format!(
                "grok still had unread background results after {GROK_MAX_WAKES} wake-ups: {}",
                names.join(", ")
            );
            eprintln!("grok: {reason} - giving up on session {sid}");
            out.digest = Some(wake_failure_digest(reason, out.digest.take()));
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
                out.digest = Some(wake_failure_digest(
                    format!("invalid wake session id: {e}"),
                    out.digest.take(),
                ));
                break;
            }
        };
        let (next, next_turns, next_usage) = match run_grok_turn(
            &wake,
            selection,
            sandbox,
            false,
            env,
            &prompt,
            project_root,
            None,
            runtime,
            _marker.as_mut(),
        )
        .await
        {
            Ok(next) => next,
            Err(e) => {
                // Keep what the run already has: `invoke` drops the session id
                // on `Err`, which would cost the sidecar row and any resume.
                let reason = format!("wake-up turn for session {sid} failed: {e:#}");
                eprintln!("grok: {reason}");
                out.digest = Some(wake_failure_digest(reason, out.digest.take()));
                break;
            }
        };
        cost = match (cost, next.served.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        turns += next_turns;
        usage.input_tokens += next_usage.input_tokens;
        usage.cached_input_tokens += next_usage.cached_input_tokens;
        usage.output_tokens += next_usage.output_tokens;
        usage.reasoning_output_tokens += next_usage.reasoning_output_tokens;
        // Every turn's leftovers stay on the run, whichever turn answers.
        let mut next = next;
        let mut leftovers = std::mem::take(&mut out.leftovers);
        leftovers.merge(std::mem::take(&mut next.leftovers));
        out = RunOutput {
            // The wake turn's report is the run's answer; the id stays the
            // first turn's (a wake resumes that same session).
            session_id: out.session_id,
            leftovers,
            ..next
        };
    }
    out.served.cost_usd = cost;
    // Whatever digest the run ends with describes the whole run, not its last
    // grok process.
    if let Some(d) = out.digest.as_mut() {
        d.turns = turns;
        d.usage = usage;
    }
    if wakes > 0 {
        out.text = format!(
            "(grok woken {wakes} time(s) to read background results it had not waited for)\n{}",
            out.text
        );
    }
    Ok(out)
}

/// Mark a verification failure without rewriting the provider's answer,
/// stated reason, or observed process status. No digest from run_grok_turn
/// means it captured an answer and observed a successful exit.
fn wake_failure_digest(reason: String, last: Option<Digest>) -> Digest {
    let mut digest = last.unwrap_or_else(|| Digest {
        captured: true,
        turn_error: None,
        ..no_answer_digest(String::new(), Some(&std::process::ExitStatus::default()))
    });
    digest.review_error = Some(reason);
    digest
}

/// One grok process: a single headless turn.
#[allow(clippy::too_many_arguments)]
async fn run_grok_turn(
    session: &SessionStart,
    selection: &super::ModelSpec,
    sandbox: Option<&str>,
    trust: bool,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
    launched: Option<LaunchSignal>,
    runtime: &ProviderRuntime,
    marker: Option<&mut crate::inflight::Guard>,
) -> Result<(RunOutput, u32, Usage)> {
    selection.validate(Provider::Grok)?;
    session.validate(Provider::Grok)?;
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
    // announced by `invoke`, which has also refused the cases where the grant
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

    let mut cmd = runtime.commands.get(Provider::Grok).build();
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
    cmd.env("GROK_FOREGROUND_BLOCK_BUDGET_MS", &wait_ms)
        .env("GROK_MAX_FOREGROUND_BLOCK_MS", &wait_ms);
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    let grok = state::Grok::resolve(env).ok();
    // Re-checked immediately before spawn. `check_trust` in `invoke` and grok's
    // own grant are two separate reads of the store, and grok's grant overwrites
    // a `trusted = false` - so a "no" recorded between them (by the operator, or
    // another grok) would be overturned. This cannot close that race from
    // outside grok, which offers no "grant unless declined" operation; it
    // shrinks the window from the whole setup to the time grok takes to start.
    if trust && let Some(ref g) = grok {
        g.check_trust(project_root)?;
    }
    // Check the runner's stop channel immediately before launch. The keeper
    // gate below also checks process-wide shutdown under its registry lock,
    // closing the race between this check and spawn.
    if *runtime.stop.borrow() {
        anyhow::bail!("not launched: review was signalled to stop");
    }
    let label = match &fresh_id {
        Some(id) => format!("the grok run of session {id}"),
        None => format!("the grok run of session {session_id}"),
    };
    let mut contained = crate::contained::spawn(
        &cmd,
        Stdio::null(),
        &runtime.teardown,
        &label,
        runtime.orphan_log.clone(),
    )?;
    if let Some(marker) = marker {
        marker.record_keeper(contained.pid());
    }
    let stdout_pipe = contained
        .child
        .stdout
        .take()
        .context("failed to open grok stdout")?;
    let stderr_pipe = contained
        .child
        .stderr
        .take()
        .context("failed to open grok stderr")?;
    // Read while the run goes, not after: the keeper is reaped only once the
    // teardown is done, and a reader started late could leave grok blocked on
    // a full pipe.
    let stdout_shared = new_shared_buf();
    let stderr_shared = new_shared_buf();
    let stdout_task = tokio::spawn(read_capped(
        stdout_pipe,
        STDOUT_CAPTURE_CAP,
        std::sync::Arc::clone(&stdout_shared),
    ));
    let stderr_task = tokio::spawn(read_capped(
        stderr_pipe,
        STDERR_CAPTURE_CAP,
        std::sync::Arc::clone(&stderr_shared),
    ));
    let mut contained = contained.launch().await?;
    if let Some(signal) = launched {
        let _ = signal.send(());
    }
    let keeper_status = contained.wait().await;
    let finished = contained.finish(&keeper_status).await;
    let leftovers = finished.leftovers;
    // `None` when the keeper could not report it; what grok printed still
    // stands, and the ledger fails the command for the unknown teardown.
    let status = finished.status;
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
            session_id: keep_id(answer.session_id),
            codex_home: None,
            served,
            leftovers,
            // A real answer from a process that nonetheless exited non-zero is
            // still a real answer, but the status is worth surfacing rather
            // than discarding - it is the only sign that something went wrong
            // after the turn finished. `captured: true` keeps it out of
            // `died_without_answer`, so the run does not fail on it.
            digest: answered_status_digest(status.as_ref()).map(with_usage),
        },
        GrokOutcome::NoAnswer {
            reason,
            review_error,
            text,
            session_id,
        } => {
            // The result object says only `cancelled`; grok's own event log
            // says why, and folder trust is the cause worth naming outright.
            let cause = grok.as_ref().and_then(|g| {
                let sid = session_id.as_deref().unwrap_or(&lookup_id);
                state::session_events(&g.home, sid)
                    .as_deref()
                    .and_then(state::cancel_cause)
            });
            let untrusted = grok.as_ref().and_then(|g| g.untrusted_hint(project_root));
            let reason = explain_grok_no_answer(reason, cause.as_deref(), untrusted.as_deref());
            let digest = if review_error {
                review_no_answer_digest(reason, status.as_ref())
            } else {
                no_answer_digest(reason, status.as_ref())
            };
            RunOutput {
                text,
                session_id: keep_id(session_id),
                codex_home: None,
                digest: Some(with_usage(digest)),
                served,
                leftovers,
            }
        }
    };
    // Turn and token counts go back separately: a clean answer carries no
    // digest, and `run_grok` needs every turn's counts to describe the run.
    Ok((run, turns, usage))
}

/// Extend grok's bare stop reason with what its event log and folder trust
/// say. Pure so the wording is tested without a grok home.
fn explain_grok_no_answer(reason: String, cause: Option<&str>, untrusted: Option<&str>) -> String {
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
        Usage {
            input_tokens: u.input_tokens + u.cache_read_input_tokens,
            cached_input_tokens: u.cache_read_input_tokens,
            output_tokens: u.output_tokens,
            reasoning_output_tokens: u.reasoning_tokens,
        },
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
    Resume(String),
    /// A resume of a session created under grok's built-in `workspace`, before
    /// `review` made profiles. It can only continue as it began: trusted, but
    /// without the extra roots.
    BuiltinWorkspace,
}

/// Generated profiles use exactly the stable suffix emitted by
/// `state::profile_name`. Other names sharing the prefix belong to the
/// operator and are passed to Grok as native profiles.
pub(super) fn is_generated_profile_name(name: &str) -> bool {
    name.strip_prefix(state::PROFILE_PREFIX)
        .is_some_and(|suffix| {
            suffix.len() == 16
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// What `prepare_grok_write` decided.
#[derive(Debug, Default)]
pub(super) struct GrokPrepared {
    /// The `review-ws-*` profile to launch under, when the run uses one.
    pub(super) profile: Option<String>,
    /// The trust key `--trust` will grant, when the project had no decision
    /// yet. `None` when it was already trusted: no flag, no grant, nothing to
    /// announce.
    pub(super) trust_grant: Option<String>,
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
pub(super) fn prepare_grok_write(
    kind: &GrokWrite,
    project_root: &Path,
    roots: &[String],
    env: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<GrokPrepared> {
    let grok = state::Grok::resolve(env)?;
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
        prepared.trust_grant = Some(key.to_string_lossy().into_owned());
    }
    let name = match kind {
        GrokWrite::BuiltinWorkspace => return Ok(prepared),
        GrokWrite::Fresh => state::profile_name(roots),
        GrokWrite::Resume(name) => {
            // The recorded roots must be the set the session's profile is named
            // for; anything else is a row edited by hand or a record from a
            // different run, and launching would grant a set nobody recorded.
            if state::profile_name(roots) != *name {
                anyhow::bail!(
                    "the roots recorded with this session do not match its grok \
                     sandbox profile `{name}` - start a fresh run"
                );
            }
            name.clone()
        }
    };
    // Recreated on resume if someone deleted it: the name pins the exact set,
    // so recreating it grants nothing the session did not already have.
    if grok.ensure_profile(&name, roots)? {
        eprintln!(
            "grok: created sandbox profile `{name}` in {} (writable roots: {})",
            grok.home.join("sandbox.toml").display(),
            if roots.is_empty() {
                "none beyond grok's workspace".to_string()
            } else {
                roots.join(", ")
            }
        );
    }
    prepared.profile = Some(name);
    Ok(prepared)
}

/// Whether the project is now trusted in grok's store, as the child's
/// environment resolves it. Read after a run that passed `--trust`, because a
/// grant grok accepted for the session is not proof that it was persisted.
pub(super) fn trust_landed(
    env: Option<&std::collections::BTreeMap<String, String>>,
    project_root: &Path,
) -> bool {
    state::Grok::resolve(env)
        .is_ok_and(|g| matches!(g.check_trust(project_root), Ok((_, state::Trust::Trusted))))
}

#[cfg(test)]
mod tests;
