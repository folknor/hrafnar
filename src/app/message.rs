//! Session steering, validation, and inherited launch settings.

use anyhow::{Result, bail};

use super::{format_runtime, print_left_running, run_failed, selected_root};
// Interrupt remains a separate verb owner; message composes its signalling
// and wording without duplicating it or moving storage policy into the app.
use crate::{
    config, config_write, harness, inflight, input, interrupt_cmd, lock, project, sessions,
};

/// The harness a `review resume` talks to: the one the sidecar recorded for the
/// session.
///
/// A session belongs to exactly one harness, and the only place an operator
/// gets a session ID from is `review`'s own output, whose run wrote that record.
/// So there is nothing to ask for: no `--harness`, and no scanning harness
/// session stores. No record means the session is not one this host's `review`
/// created - another host's (whose harness store is not here either), one made
/// outside `review`, or a lost log - and that is an error, not a guess.
///
/// A recorded harness is still parsed, so a row naming a harness this build
/// does not drive cannot turn into an unrunnable invocation.
///
/// Recognising the name is not the same as the current runner understanding
/// the session. Every recognised name has had one integration, so the two
/// coincide. They stop coinciding if a name that rows already record is ever
/// given a new integration: rows the earlier one wrote would parse as the new
/// one. The stale-session gate does
/// not cover that - rows with no usable epoch bypass it - so re-adding a name
/// must come with a compatibility check here, a minimum `review_version` per
/// harness below which rows are refused, with the crate version bumped past
/// every build of the old integration (rows record `CARGO_PKG_VERSION`), and
/// a missing or malformed version refused.
fn resume_record(
    session_id: &str,
    record: Option<sessions::SessionRecord>,
) -> Result<(sessions::SessionRecord, harness::Harness)> {
    let record = record.ok_or_else(|| sessions::query::not_found(session_id))?;
    let Ok(harness) = record.harness.parse::<harness::Harness>() else {
        bail!(
            "session {session_id} was recorded for harness '{}', which review no \
             longer supports\n  supported: {}",
            record.harness,
            harness::Harness::supported()
        );
    };
    Ok((record, harness))
}

/// The project a message to `session_id` runs in, with its config: the one
/// the session was started in, never the caller's directory.
///
/// Deciding it from the cwd would run a message sent from another checkout
/// there, under that checkout's audit id and privacy setting, and file the row
/// under it, after which the next resume would inherit the misfiled row. So
/// the project comes from the session's record (or, for a fresh run still in
/// flight, its marker) and `-C` can only confirm it.
///
/// Everything is checked before anything is interrupted: the recorded root
/// must still exist and hold its `.review.toml` directly - every recorded run
/// required one, so its absence is drift, and walking up instead would adopt an
/// ancestor's audit id and privacy - and that config must parse. The audit id
/// must match the record too: a path alone cannot tell the original checkout
/// from another project initialized in its place.
struct MessageTarget {
    config: config::ReviewConfig,
}

/// A snapshot to compare with a newly loaded target after interruption. Keeping
/// this distinct from launch policy prevents reusing the first filesystem check.
#[derive(PartialEq, Eq)]
struct TargetIdentity {
    root: project::ProjectRoot,
    audit: Option<crate::data::AuditId>,
}

impl MessageTarget {
    fn identity(&self) -> Result<TargetIdentity> {
        Ok(TargetIdentity {
            root: self.config.require_project_root()?.clone(),
            audit: self.config.audit.id.clone(),
        })
    }
}

fn message_target(
    session_id: &str,
    record: Option<&sessions::SessionRecord>,
    marker_project: Option<&str>,
    selector: Option<&str>,
) -> Result<MessageTarget> {
    let recorded = match (record.map(|r| r.project.as_str()), marker_project) {
        (Some(r), Some(m)) if !project::same_project(r, std::path::Path::new(m)) => bail!(
            "session {session_id} is recorded in {r} but its turn in flight runs in {m}\n  \
             Refusing to guess which project the message belongs to."
        ),
        (Some(p), _) | (None, Some(p)) => p,
        (None, None) => return Err(sessions::query::not_found(session_id)),
    };
    let root = std::path::PathBuf::from(recorded);
    if !root.is_dir() {
        bail!(
            "session {session_id} belongs to {recorded}, which no longer exists\n  \
             `review sessions {session_id}` still shows what it recorded."
        );
    }
    if let Some(sel) = selector {
        let selected = selected_root(sel)?;
        sessions::query::assert_project(session_id, recorded, selected.as_path(), sel)?;
    }
    let cfg = config::load(&config::Selection::Exact(root))?;
    let root = cfg.require_project_root()?.as_path();
    if let Some(rec) = record
        && let Some(recorded_id) = crate::data::AuditId::from_record(&rec.audit_id)
        && cfg.audit.id.as_ref() != Some(&recorded_id)
    {
        // Empty historical ids carry no identity assertion.
        let current_audit_id = cfg.audit.id.as_deref().unwrap_or("none");
        bail!(
            "session {session_id} was recorded under audit id {} but {} now has {}\n  \
             The checkout at that path is not the project the session was started in.",
            rec.audit_id,
            root.display(),
            current_audit_id
        );
    }
    Ok(MessageTarget { config: cfg })
}

/// Whether the session's own fresh launch (its `run` row) authenticated with
/// `claude_token_command`. Later rows only report what each attempt used, so
/// they neither grant nor revoke this; a session whose run row predates the
/// field carries no such policy.
fn session_uses_token_command(
    snapshot: &sessions::query::Snapshot,
    record: &sessions::SessionRecord,
    project_root: &std::path::Path,
) -> bool {
    // Bound to the session this message was validated against - its audit
    // id, this host - so a row from another host or audit trail sharing the
    // data directory cannot opt this continuation into the local command.
    let host = crate::config::hostname();
    // An empty historical id identifies nothing, so it cannot bind a policy.
    if record.audit_id.is_empty() {
        return false;
    }
    snapshot.find(&record.session_id).is_some_and(|session| {
        session.rows.iter().any(|row| {
            row.kind == sessions::SessionKind::Run
                && row.harness == harness::Harness::Claude.as_str()
                && row.hostname == host
                && row.audit_id == record.audit_id
                && crate::project::same_project(&row.project, project_root)
                && row.credential.as_deref()
                    == Some(harness::claude_token::CREDENTIAL_TOKEN_COMMAND)
        })
    })
}

/// `review message <id>`: deliver stdin to a session whatever it is doing. A
/// turn in flight on an interruptible harness is ended first, exactly as
/// `review interrupt` ends it, and the message then continues the session as a
/// resume. How the launch lock, marker, interrupt request and handoff keep two
/// messages from running beside each other is in `reference/steering.md`.
///
/// The verb exists for its name. Told to "interrupt the agent", an
/// orchestrating Claude reached for its own task-stop tool instead, killing
/// `review` rather than the turn, which left a fresh session with no record to
/// resume. "Message the agent" has no such rival.
pub(super) async fn run_message(
    session_id: &str,
    dry_run: bool,
    selector: Option<&str>,
    started: std::time::Instant,
    lifecycle: &crate::supervisor::Lifecycle,
) -> Result<crate::exit::CommandExit> {
    // Reject unsafe executable identities before locks or interruption. Rows
    // remain permissive historical evidence and are not changed on disk.
    let validated_id = harness::SessionId::parse(session_id)?;
    // Read before interrupting anything: a missing message must not cost the
    // turn in flight.
    let instructions = input::read_stdin()?;
    // Held from before the in-flight check until this message's run has
    // launched (see `inflight::LaunchLock`): a second message to the session
    // waits here, then finds this one's turn in flight and interrupts it,
    // instead of both launching on one idle session. Blocking, so off the
    // async runtime.
    let launch_lock = if dry_run {
        None
    } else {
        let sid = session_id.to_string();
        tokio::task::spawn_blocking(move || inflight::lock_session_launch(&sid, None))
            .await
            .map_err(|e| anyhow::anyhow!("launch lock task failed: {e}"))??
    };
    // Read on a dry run too: the marker can name the project (a fresh run
    // still in flight has no row yet) or contradict the row, and a preview
    // that skipped it would pass what the real message refuses.
    // Observe first: pending-marker removal follows durable recording. If we
    // see no marker after that removal, history must be acquired afterwards.
    let observation = sessions::query::Observation::capture(None);
    let mut snapshot = sessions::query::Snapshot::read();
    let marker = match observation.state(session_id, &snapshot) {
        sessions::query::State::InFlight(m)
        | sessions::query::State::CleanupPending(m)
        | sessions::query::State::HandoffPending(m) => Some(m.clone()),
        sessions::query::State::Ambiguous => {
            bail!("session {session_id}: ambiguous live-run observation; retry shortly")
        }
        sessions::query::State::Queued(row) => {
            let _ = row;
            None
        }
        sessions::query::State::Idle(row) => {
            let _ = row;
            None
        }
        sessions::query::State::Unknown => None,
    };
    let record = snapshot.latest(session_id).cloned();
    // Under the launch lock, before any signal: a message bound for the wrong
    // project, or one whose project config is broken, must not cost the turn.
    let target = message_target(
        session_id,
        record.as_ref(),
        marker.as_ref().map(|m| m.project.as_str()),
        selector,
    )?;
    if dry_run && record.is_none() {
        // Only a marker so far: nothing to inherit or age-check yet, so the
        // preview is the message itself.
        eprintln!("session: {session_id} (turn in flight, not yet recorded)");
        eprintln!(
            "project: {}",
            target.config.require_project_root()?.as_path().display()
        );
        println!("{instructions}");
        return Ok(crate::exit::CommandExit::Success);
    }
    let expected = target.identity()?;
    if let Some(marker) = marker.filter(|_| !dry_run) {
        // Its owner is gone and its processes are still being torn down: a
        // turn launched now would run beside them.
        if marker.cleanup_pending() {
            bail!(
                "{}",
                interrupt_cmd::cleanup_pending_message(session_id, &marker)
            );
        }
        if marker.phase == inflight::Phase::Running
            && !interrupt_cmd::interruptible(&marker.harness)
        {
            bail!(
                "session {session_id} has a {} turn in flight, which cannot be interrupted\n  \
                 send the message once it has finished",
                marker.harness
            );
        }
        if marker.phase == inflight::Phase::HandoffPending {
            eprintln!("session {session_id} is being recorded; waiting for its handoff");
        } else {
            eprintln!("session {session_id} has a turn in flight; interrupting it first");
        }
        let outcome = interrupt_cmd::interrupt_observed(session_id, &marker, None).await?;
        snapshot = sessions::query::Snapshot::read();
        if !outcome.interrupted {
            eprintln!(
                "the turn finished before the interrupt took effect - its answer is in \
                 `review sessions {session_id}`"
            );
        }
    }
    // Looked up once and reused by the cache-age gate and inheritance below.
    let (record, harness_name) = resume_record(session_id, snapshot.latest(session_id).cloned())?;
    eprintln!("harness: {harness_name} (from the session record)");
    let archetype = record.archetype.as_str();

    let stdin_instructions = instructions;
    // Checked again on the record as it is now: an interrupted fresh run has
    // only just written its first row, which `run_message` could not see. The
    // config decides whether this resume's rows go to the private log, so one
    // that does not parse is an error, never treated as absent.
    let target = message_target(session_id, Some(&record), None, selector)?;
    let actual = target.identity()?;
    let cfg = target.config;
    let project_root = actual.root.to_path_buf();
    // And it must still be the project validated before the interrupt: the
    // row the interrupted run wrote is checked against its own config above,
    // which on its own would accept a row naming some other valid project.
    if expected != actual {
        bail!(
            "session {session_id} now records {} (audit id {}), not {} as it did before \
             the interrupt\n  Nothing was sent.",
            project_root.display(),
            cfg.audit.id.as_deref().unwrap_or("none"),
            expected.root.as_path().display()
        );
    }

    if dry_run {
        eprintln!("session: {session_id}");
        eprintln!("archetype: {archetype}");
        println!("{stdin_instructions}");
        return Ok(crate::exit::CommandExit::Success);
    }

    if !harness::is_available(harness_name) {
        bail!("'{harness_name}' not found on PATH");
    }

    // Cache-age gate. The sidecar tells us how long it's been since the session
    // last ended; past the harness's cutoff (`Harness::stale_cutoff`: 27 min
    // for codex, whose prompt cache lasts ~30 min, ~55 min otherwise) the cache
    // is cold, and resuming means reprocessing the whole session prefix at full
    // cost. Resuming is the *warm* follow-up path, so a cold resume is refused:
    // do a fresh run with restated context instead. A row with no usable
    // timestamp leaves the age unknown, and the resume proceeds rather than blocks.
    let age = sessions::Age::since_epoch(record.epoch_secs);
    if let Some(elapsed) = age.elapsed_secs() {
        if elapsed > harness_name.stale_cutoff().as_secs() {
            bail!(
                "session last touched {age} - its prompt cache is cold.\n  \
                 Resuming would reprocess the whole session prefix at full cost.\n  \
                 Start a fresh run with restated context instead."
            );
        }
        eprintln!("session last touched {age}");
    }

    // A claude session created with a token from `claude_token_command`
    // continues on that credential source, read from its `run` row - the one
    // its fresh launch wrote, found by kind rather than by timestamp. Fetched
    // here, before the global lock, so a slow helper holds nothing; and a
    // session that needs it but cannot get it is refused before anything is
    // sent, since a failed attempt would leave the prompt in its transcript.
    let claude_token = if harness_name == harness::Harness::Claude
        && session_uses_token_command(&snapshot, &record, &project_root)
        && !harness::claude_token::env_has_token(None)
    {
        let Some(command) = cfg.claude_token_command.clone() else {
            bail!(
                "session {session_id} was created with a token from claude_token_command, which \
                 the global config no longer sets\n  Nothing was sent."
            );
        };
        let source = harness::claude_token::TokenSource::new(command);
        let fetched = tokio::task::spawn_blocking(move || source.fetch())
            .await
            .map_err(|e| anyhow::anyhow!("claude_token_command task failed: {e}"))?;
        match fetched {
            Ok(token) => {
                eprintln!("credential: claude_token_command (inherited from the session record)");
                Some(token)
            }
            Err(why) => bail!(
                "session {session_id} needs a token from claude_token_command: {why}\n  Nothing was sent."
            ),
        }
    } else {
        None
    };

    // Global lock: serialize the *launch* against other `review` invocations,
    // as a fresh run does (it releases the lock after its launch spacing, not
    // when the run finishes).
    //
    // It is deliberately NOT held across the run. The lock exists to space out
    // harness launches, and a resume launches exactly one thing, so there is
    // nothing left to serialize once it is running. Holding it for the turn's
    // duration meant a single wedged resume froze all `review` traffic on the
    // host indefinitely - every later invocation sat waiting on the lock
    // behind a process that would never return.
    // A hung run should cost you that run, not the tool.
    //
    // It *is* held across the spawn, via the launch handshake: releasing it
    // before calling `invoke` would leave a critical section guarding nothing,
    // letting every queued resume through to spawn simultaneously - the rate
    // limiting the lock exists to provide.
    // Held to the end, past the sidecar row: see `run_launch`.
    let _unrecorded = lifecycle.unrecorded_for(harness_name);
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = lock::open_lock_file(&lock_path)?;
    lock::acquire_blocking(&lock_file)?;
    // Persist an id before launching so every concurrent resume records the
    // same project identity, and a write failure cannot leave an unidentifiable
    // session behind.
    let audit_id = config_write::ensure_audit_id(&project_root.join(".review.toml"))?;
    let scope = crate::data::LogScope::new(audit_id, cfg.audit.private);
    let data = crate::data::DataDir::process().clone();
    // Prompt/profile settings are inherited; the current stall safety setting is honoured.
    let harness_runtime = harness::HarnessRuntime::from_config(
        lifecycle.clone(),
        cfg.stall_timeout(),
        cfg.claude_token_command.as_deref(),
        data.clone(),
        &scope,
    );

    // Permissions, model and effort are inherited from the session's own last
    // recorded run rather than defaulted (see `InheritedSettings::from_record`). Each is
    // announced, because the failure this replaced was entirely silent: the
    // resume simply ran on a different model and nothing said so.
    let inherited = harness::InheritedSettings::from_record(Some(&record), harness_name);
    if let Some(level) = inherited
        .sandbox
        .as_ref()
        .and_then(harness::Permissions::selector)
    {
        eprintln!("sandbox: {level} (inherited from the session record)");
    }
    if let Some(ref m) = inherited.model {
        eprintln!("model: {m} (inherited from the session record)");
    }
    if let Some(ref e) = inherited.effort {
        eprintln!("effort: {e} (inherited from the session record)");
    }

    // A session created under a profile CODEX_HOME must resume against that
    // same storage directory. Older rows have no saved path and keep using the
    // current process environment/default.
    let mut settings = harness::RunSettings::from_inherited(harness_name, inherited);
    if let Some(token) = claude_token {
        settings = settings.with_claude_token(token);
    }
    let spec = harness::LaunchSpec {
        harness: harness_name,
        start: harness::SessionStart::Resume(validated_id),
        settings,
        prompt: stdin_instructions.clone(),
        root: project_root.clone(),
    };
    let (launched_tx, launched_rx) = tokio::sync::oneshot::channel();
    let invoke = harness::invoke(spec, Some(launched_tx), &harness_runtime, None);
    tokio::pin!(invoke);

    // Release the lock as soon as the process is spawned. `launched_rx` also
    // resolves (as an error) if the sender is dropped without sending, i.e. the
    // spawn failed - which is equally a reason to stop holding the lock. The
    // `invoke` arm covers a harness that returns before signalling at all.
    // The session's launch lock goes at the same moment: once the harness has
    // spawned, its in-flight marker exists, so a later message to this session
    // sees the turn in flight.
    let result = tokio::select! {
        result = &mut invoke => {
            drop(lock_file);
            drop(launch_lock);
            result
        }
        _ = launched_rx => {
            drop(lock_file);
            drop(launch_lock);
            invoke.await
        }
    };

    let ctx = super::launch::RecordCtx {
        project_root,
        data,
        scope,
        archetype: archetype.to_owned(),
        // Manual messages do not restore profile env; record what this launch
        // actually used, never copy names for values it did not inherit.
        env_keys: Vec::new(),
        profile_origin: None,
        operator_prompt: stdin_instructions.clone(),
    };
    let recorded = ctx.record_message(&result, &stdin_instructions);
    let _ = ctx.publish(&result, &result, recorded);

    if result
        .output
        .as_ref()
        .is_ok_and(|turn| turn.outcome.auth.is_some())
    {
        eprintln!(
            "claude could not authenticate. A message is never resent (the prompt is already in \
             the session's transcript). If claude is logged in through CLAUDE_CODE_OAUTH_TOKEN, \
             Claude Code does not pass it to the commands it runs: send this from a terminal; a \
             session started with [_defaults].claude_token_command gets the token up front."
        );
    }
    harness::print_result(&result);
    let left_running = print_left_running(lifecycle);
    println!("\nruntime: {}", format_runtime(started.elapsed()));

    // A stalled or dead resume comes back as `Ok` (so its session id, digest and
    // incident survive), but it is a failure for exit-code purposes - exiting 0
    // on a wedged run lies to scripts and CI, as it would on a fresh run.
    // Anything left running outranks it.
    Ok(crate::exit::CommandExit::of_runs(
        run_failed(&result),
        left_running,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_row(harness: &str) -> sessions::SessionRecord {
        record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": harness,
            "archetype": "security",
            "session_id": "s",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }))
    }

    #[test]
    fn a_resume_takes_the_harness_from_the_record() {
        // The session ID came from review's own output, whose run wrote this
        // row, so the harness is on record and is never asked for.
        let (rec, harness) = resume_record("s", Some(minimal_row("codex"))).expect("resolved");
        assert_eq!(harness, harness::Harness::Codex);
        assert_eq!(rec.archetype, "security");
        assert_eq!(rec.codex_home, None);
    }

    fn resume_error(record: Option<sessions::SessionRecord>) -> String {
        match resume_record("s", record) {
            Ok(_) => panic!("expected the resume to be refused"),
            Err(e) => e.to_string(),
        }
    }

    /// A sidecar row, built the way one is actually read back - from JSON, so a
    /// field this test forgets is exercised as a missing field rather than as a
    /// compile error the struct literal would have caught.
    fn record(json: serde_json::Value) -> sessions::SessionRecord {
        serde_json::from_value(json).expect("valid session record")
    }

    #[test]
    fn grok_resume_prefers_native_selector_and_accepts_old_rows() {
        for native in ["workspace", "none", "review-ws-abc", "custom"] {
            let mut rec = minimal_row("grok");
            rec.sandbox = Some(native.to_string());
            assert_eq!(
                harness::InheritedSettings::from_record(Some(&rec), harness::Harness::Grok)
                    .sandbox
                    .as_ref()
                    .and_then(harness::Permissions::selector),
                Some(native)
            );
            rec.harness_sandbox = Some(native.to_string());
            rec.sandbox =
                harness::Permissions::restore(harness::Harness::Grok, Some(native), None, &[])
                    .recorded_level()
                    .map(str::to_string);
            assert_eq!(
                harness::InheritedSettings::from_record(Some(&rec), harness::Harness::Grok)
                    .sandbox
                    .as_ref()
                    .and_then(harness::Permissions::selector),
                Some(native)
            );
        }
    }

    #[test]
    fn a_resume_inherits_the_permissions_of_the_run_that_created_the_session() {
        let rec = record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": "codex",
            "archetype": "implement",
            "session_id": "s",
            "kind": "run",
            "sandbox": "workspace-write",
            "writable_roots": ["/home/u/.brokkr", "/srv/cache"],
            "model": "gpt-5.6-sol",
            "effort": "low",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }));
        let got = harness::InheritedSettings::from_record(Some(&rec), harness::Harness::Codex);
        assert_eq!(
            got.sandbox
                .as_ref()
                .and_then(harness::Permissions::selector),
            Some("workspace-write")
        );
        assert_eq!(got.writable_roots, vec!["/home/u/.brokkr", "/srv/cache"]);
        assert_eq!(got.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(got.effort.as_deref(), Some("low"));
    }

    #[test]
    fn a_row_recording_a_model_but_no_effort_inherits_just_the_model() {
        // Rows written before `effort` was recorded carry a model and nothing
        // else. Inheriting the model is still strictly better than falling back
        // to the harness's default model, so the absent effort must not
        // suppress it.
        let rec = record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": "codex",
            "archetype": "implement",
            "session_id": "s",
            "model": "gpt-5.6-sol",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }));
        let got = harness::InheritedSettings::from_record(Some(&rec), harness::Harness::Codex);
        assert_eq!(got.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(got.effort, None);
    }

    #[test]
    fn a_row_from_before_permissions_were_recorded_inherits_nothing() {
        // The pre-existing default (read-only, no roots) has to survive an old
        // row, since the fields are simply absent there.
        let rec = record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": "codex",
            "archetype": "implement",
            "session_id": "s",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }));
        let got = harness::InheritedSettings::from_record(Some(&rec), harness::Harness::Codex);
        assert_eq!(
            got.sandbox
                .as_ref()
                .and_then(harness::Permissions::selector),
            Some("read-only")
        );
        assert!(got.writable_roots.is_empty());
        assert_eq!(got.model, None);
        assert_eq!(got.effort, None);
    }

    #[test]
    fn permissions_are_not_inherited_across_harnesses() {
        // `resume_record` takes the harness from the same row, so this cannot
        // happen today, but a grok level fed to codex (or vice versa) is a
        // widening nobody asked for, so the guard is here rather than assumed
        // upstream.
        let rec = record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": "grok",
            "archetype": "implement",
            "session_id": "s",
            "sandbox": "none",
            "model": "grok-4",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }));
        let got = harness::InheritedSettings::from_record(Some(&rec), harness::Harness::Codex);
        assert_eq!(got.sandbox, None);
        assert!(got.writable_roots.is_empty());
        // The model is harness-scoped too: a grok model name handed to codex
        // is not a fallback, it is an invocation that cannot start.
        assert_eq!(got.model, None);
    }

    #[test]
    fn no_record_inherits_nothing() {
        let got = harness::InheritedSettings::from_record(None, harness::Harness::Codex);
        assert_eq!(got.sandbox, None);
        assert!(got.writable_roots.is_empty());
        assert_eq!(got.model, None);
        assert_eq!(got.effort, None);
    }

    #[test]
    fn a_resume_with_no_record_is_refused_and_points_at_sessions() {
        let err = resume_error(None);
        assert!(err.contains("review sessions"), "{err}");
    }

    #[test]
    fn a_recorded_harness_review_no_longer_supports_is_refused() {
        // A row naming a harness this build does not drive must not turn into
        // an unrunnable invocation.
        let err = resume_error(Some(minimal_row("opencode")));
        assert!(err.contains("supported"), "{err}");
    }
}
