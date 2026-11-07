//! Session steering, validation, and inherited launch settings.

use anyhow::{Result, bail};

use super::{exit_status, format_runtime, print_left_running, run_failed, selected_root};
use crate::{
    audit, config, config_write, inflight, input, interrupt_cmd, lock, project, provider, sessions,
    timings,
};

/// The provider a `review resume` talks to: the one the sidecar recorded for the
/// session.
///
/// A session belongs to exactly one provider, and the only place an operator
/// gets a session ID from is `review`'s own output, whose run wrote that record.
/// So there is nothing to ask for: no `--provider`, and no scanning provider
/// session stores. No record means the session is not one this host's `review`
/// created - another host's (whose provider store is not here either), one made
/// outside `review`, or a lost log - and that is an error, not a guess.
///
/// A recorded provider is still validated, so a row naming a removed provider
/// (kilo/opencode) cannot turn into an unrunnable invocation.
fn resume_record(
    session_id: &str,
    record: Option<sessions::SessionRecord>,
) -> Result<sessions::SessionRecord> {
    let Some(record) = record else {
        bail!(
            "no record of session {session_id} on this host\n  \
             `review sessions` lists the sessions this host's review runs created."
        );
    };
    if !config::KNOWN_PROVIDERS.contains(&record.provider.as_str()) {
        bail!(
            "session {session_id} was recorded for provider '{}', which review no \
             longer supports\n  supported: {}",
            record.provider,
            config::KNOWN_PROVIDERS.join(", ")
        );
    }
    Ok(record)
}

/// What a `review resume` launches with, taken from the session's own last
/// recorded run: sandbox level, writable roots, model and reasoning effort.
///
/// A resume carries no profile, and the runners default to `read-only`, so a
/// resume used to silently drop the permissions the session was created with: a
/// `workspace-write` run that died mid-turn came back as a read-only resume that
/// could not touch the files it had been editing, and the failure surfaced as
/// the model reporting a read-only filesystem rather than as anything about
/// `review`. The permissions are a property of the *session*, not of the
/// invocation that happens to be driving it.
///
/// The sidecar already records both, and records the **effective** values (what
/// the run received, not what its profile asked for), which is exactly what has
/// to carry forward - the recurring lesson of the codex sandbox work is that
/// those two differ. Roots are passed as profile roots rather than as a verbatim
/// list so they go back through the same filters and are re-derived against
/// *this* host: a host fact like the build lock belongs to the machine the
/// resume runs on, not to the one that recorded the row.
///
/// Known levels are recorded in review's vocabulary. Grok also saves its
/// native selector in `provider_sandbox`, since a session cannot change profiles.
/// Old rows have only `sandbox`, already in Grok's native form, so that field
/// remains the fallback. A row predating both fields inherits nothing.
///
/// Model and effort carry forward for the same reason and were originally left
/// out on the grounds that only `model` was recorded, so a partial restoration
/// would be less predictable than none. Measurement killed that argument: every
/// resume ran on codex's **built-in default model** at its default
/// effort, not the profile's, and since `--ignore-user-config` that default is
/// not even the operator's configured one. A session's first turn on
/// `gpt-5.6-sol`/`low` followed by five resumed turns on a different model is
/// not a neutral fallback - it silently changes the model mid-session, at a
/// different price, with nothing in the output saying so. The fix is to record
/// `effort` too rather than to keep inheriting neither.
///
/// Env and profile `config` are still *not* inherited: env values are
/// deliberately never recorded (they can carry secrets) and `config` is not
/// recorded at all, so there is nothing to restore from. That is an absence of
/// data, not a judgement about predictability.
fn inherited_settings(
    record: Option<&sessions::SessionRecord>,
    provider_name: &str,
) -> InheritedSettings {
    let Some(record) = record.filter(|r| r.provider == provider_name) else {
        return InheritedSettings::default();
    };
    InheritedSettings {
        sandbox: if provider_name == "grok" {
            record
                .provider_sandbox
                .clone()
                .or_else(|| record.sandbox.clone())
        } else {
            record.sandbox.clone()
        },
        // For grok these are exactly the roots the session's `review-ws-*`
        // profile is named for, and a resume uses them verbatim rather than
        // re-deriving - see `provider::grok::GrokWrite::Resume`.
        writable_roots: record.writable_roots.clone(),
        model: record.model.clone(),
        effort: record.effort.clone(),
    }
}

#[derive(Default)]
struct InheritedSettings {
    sandbox: Option<String>,
    writable_roots: Vec<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// The project a message to `session_id` runs in, with its config: the one
/// the session was started in, never the caller's directory.
///
/// The cwd used to decide it, so a message sent from another checkout ran the
/// session there, under that checkout's audit id and privacy setting, and filed
/// the row under it - after which the next resume inherited the misfiled row.
/// Now the project comes from the session's record (or, for a fresh run still
/// in flight, its marker) and `-C` can only confirm it.
///
/// Everything is checked before anything is interrupted: the recorded root
/// must still exist and hold its `.review.toml` directly - every recorded run
/// required one, so its absence is drift, and walking up instead would adopt an
/// ancestor's audit id and privacy - and that config must parse. The audit id
/// must match the record too: a path alone cannot tell the original checkout
/// from another project initialized in its place.
fn message_target(
    session_id: &str,
    record: Option<&sessions::SessionRecord>,
    marker_project: Option<&str>,
    selector: Option<&str>,
) -> Result<(config::ReviewConfig, std::path::PathBuf)> {
    let recorded = match (record.map(|r| r.project.as_str()), marker_project) {
        (Some(r), Some(m)) if !project::same_project(r, std::path::Path::new(m)) => bail!(
            "session {session_id} is recorded in {r} but its turn in flight runs in {m}\n  \
             Refusing to guess which project the message belongs to."
        ),
        (Some(p), _) | (None, Some(p)) => p,
        (None, None) => bail!(
            "no record of session {session_id} on this host\n  \
             `review sessions` lists the sessions this host's review runs created."
        ),
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
        if !project::same_project(recorded, &selected) {
            bail!(
                "session {session_id} belongs to {recorded}, not {} (-C {sel})",
                selected.display()
            );
        }
    }
    let (cfg, root) = config::load(&config::Selection::Exact(root))?;
    if let Some(rec) = record
        && !rec.audit_id.is_empty()
        && cfg.audit.id.as_deref() != Some(rec.audit_id.as_str())
    {
        // An empty configured id is absent, as it is to `ensure_audit_id`.
        let current_audit_id = cfg
            .audit
            .id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or("none");
        bail!(
            "session {session_id} was recorded under audit id {} but {} now has {}\n  \
             The checkout at that path is not the project the session was started in.",
            rec.audit_id,
            root.display(),
            current_audit_id
        );
    }
    Ok((cfg, root))
}

/// `review message <id>`: deliver stdin to a session whatever it is doing. A
/// codex or claude turn in flight is ended first, exactly as `review interrupt`
/// ends it, and the message then continues the session as a resume.
///
/// The verb exists for its name. Steering a running agent used to be
/// `review interrupt` followed by `review resume`, and told to "interrupt the
/// agent", an orchestrating Claude reached for its own task-stop tool instead -
/// killing `review` rather than the turn, which left a fresh session with no
/// record to resume. "Message the agent" has no such rival.
pub(super) async fn run_message(
    session_id: &str,
    dry_run: bool,
    selector: Option<&str>,
    started: std::time::Instant,
) -> Result<()> {
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
    let marker = inflight::live_marker(session_id, None);
    let record = sessions::latest_for_session(session_id);
    // Under the launch lock, before any signal: a message bound for the wrong
    // project, or one whose project config is broken, must not cost the turn.
    let (target_cfg, target_root) = message_target(
        session_id,
        record.as_ref(),
        marker.as_ref().map(|m| m.project.as_str()),
        selector,
    )?;
    if dry_run && record.is_none() {
        // Only a marker so far: nothing to inherit or age-check yet, so the
        // preview is the message itself.
        eprintln!("session: {session_id} (turn in flight, not yet recorded)");
        eprintln!("project: {}", target_root.display());
        println!("{instructions}");
        return Ok(());
    }
    let expected = (target_root, target_cfg.audit.id);
    if let Some(marker) = marker.filter(|_| !dry_run) {
        // Its owner is gone and its processes are still being torn down: a
        // turn launched now would run beside them.
        if marker.cleanup_pending {
            bail!(
                "{}",
                interrupt_cmd::cleanup_pending_message(session_id, &marker)
            );
        }
        if !interrupt_cmd::interruptible(&marker.provider) {
            bail!(
                "session {session_id} has a {} turn in flight, which cannot be interrupted\n  \
                 send the message once it has finished",
                marker.provider
            );
        }
        eprintln!("session {session_id} has a turn in flight; interrupting it first");
        let outcome = interrupt_cmd::interrupt(session_id, None, sessions::read_all).await?;
        if !outcome.interrupted {
            eprintln!(
                "the turn finished before the interrupt took effect - its answer is in \
                 `review sessions {session_id}`"
            );
        }
    }
    run_session_resume(
        session_id,
        dry_run,
        selector,
        started,
        Some(instructions),
        launch_lock,
        Some(expected),
    )
    .await
}

/// `review resume <id>`: continue a specific provider session and send raw
/// stdin. No prime and no profile - the session already has its grounding from
/// the run that created it, and its provider, permissions, model and effort
/// come from that run's record. The archetype recorded for the resume is the
/// session's own, so its rows stay grouped under what the session was opened as.
async fn run_session_resume(
    session_id: &str,
    dry_run: bool,
    selector: Option<&str>,
    started: std::time::Instant,
    instructions: Option<String>,
    // `review message`'s per-session lock, released with the global lock once
    // the run has launched (see `run_message`).
    launch_lock: Option<inflight::LaunchLock>,
    // The project and audit id `run_message` validated before interrupting.
    expected: Option<(std::path::PathBuf, Option<String>)>,
) -> Result<()> {
    // Looked up once and reused by the cache-age gate and inheritance below.
    let record = resume_record(session_id, sessions::latest_for_session(session_id))?;
    let provider_name = record.provider.as_str();
    eprintln!("provider: {provider_name} (from the session record)");
    let archetype = record.archetype.as_str();

    let stdin_instructions = match instructions {
        Some(text) => text,
        None => input::read_stdin()?,
    };
    // Checked again on the record as it is now: an interrupted fresh run has
    // only just written its first row, which `run_message` could not see. The
    // config decides whether this resume's rows go to the private log, so one
    // that does not parse is an error, never treated as absent.
    let (cfg, project_root) = message_target(session_id, Some(&record), None, selector)?;
    // And it must still be the project validated before the interrupt: the
    // row the interrupted run wrote is checked against its own config above,
    // which on its own would accept a row naming some other valid project.
    if let Some((root, audit)) = expected
        && (root != project_root || audit != cfg.audit.id)
    {
        bail!(
            "session {session_id} now records {} (audit id {}), not {} as it did before \
             the interrupt\n  Nothing was sent.",
            project_root.display(),
            cfg.audit.id.as_deref().unwrap_or("none"),
            root.display()
        );
    }

    if dry_run {
        eprintln!("session: {session_id}");
        eprintln!("archetype: {archetype}");
        println!("{stdin_instructions}");
        return Ok(());
    }

    if !provider::is_available(provider_name) {
        bail!("'{provider_name}' not found on PATH");
    }

    // A resume bypasses the config for prompt/profile purposes, but the
    // stall timeout is a safety setting rather than a prompt input, so it is
    // still honoured.
    let provider_runtime =
        provider::ProviderRuntime::from_config(cfg.stall_timeout_secs(), cfg.audit.private);

    // Cache-age gate. The sidecar tells us how long it's been since the session
    // last ended; past the provider's cutoff (`timings::stale_session`: 27 min
    // for codex, whose prompt cache lasts ~30 min, ~55 min otherwise) the cache
    // is cold, and resuming means reprocessing the whole session prefix at full
    // cost. Resuming is the *warm* follow-up path, so a cold resume is refused:
    // do a fresh run with restated context instead. A row with no usable
    // timestamp leaves the age unknown, and the resume proceeds rather than blocks.
    let age = sessions::Age::since_epoch(record.epoch_secs);
    if let Some(elapsed) = age.elapsed_secs() {
        if elapsed > timings::stale_session(provider_name).as_secs() {
            bail!(
                "session last touched {age} - its prompt cache is cold.\n  \
                 Resuming would reprocess the whole session prefix at full cost.\n  \
                 Start a fresh run with restated context instead."
            );
        }
        eprintln!("session last touched {age}");
    }

    // Global lock: serialize the *launch* against other `review` invocations,
    // matching what the fan-out path does (it releases the lock once its
    // staggered launches have fired, not when the runs finish).
    //
    // It is deliberately NOT held across the run. The lock exists to space out
    // provider launches, and a resume launches exactly one thing, so there is
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
    let _unrecorded = provider::Unrecorded::for_provider(provider_name);
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = lock::open_lock_file(&lock_path)?;
    lock::acquire_blocking(&lock_file)?;
    // Persist an id before launching so every concurrent resume records the
    // same project identity, and a write failure cannot leave an unidentifiable
    // session behind.
    let audit_id = config_write::ensure_audit_id(&project_root.join(".review.toml"))?;
    let private = cfg.audit.private;

    // Permissions, model and effort are inherited from the session's own last
    // recorded run rather than defaulted (see `inherited_settings`). Each is
    // announced, because the failure this replaced was entirely silent: the
    // resume simply ran on a different model and nothing said so.
    let inherited = inherited_settings(Some(&record), provider_name);
    if let Some(ref level) = inherited.sandbox {
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
    let codex_home_env = if provider_name == "codex" {
        record.codex_home.as_ref().map(|home| {
            std::collections::BTreeMap::from([("CODEX_HOME".to_string(), home.clone())])
        })
    } else {
        None
    };

    let (launched_tx, launched_rx) = tokio::sync::oneshot::channel();
    let invoke = provider::invoke(
        provider_name,
        session_id,
        inherited.model.as_deref(),
        inherited.effort.as_deref(),
        inherited.sandbox.as_deref(),
        &inherited.writable_roots,
        codex_home_env.as_ref(),
        &[],
        &stdin_instructions,
        &project_root,
        false,
        Some(launched_tx),
        &provider_runtime,
    );
    tokio::pin!(invoke);

    // Release the lock as soon as the process is spawned. `launched_rx` also
    // resolves (as an error) if the sender is dropped without sending, i.e. the
    // spawn failed - which is equally a reason to stop holding the lock. The
    // `invoke` arm covers a provider that returns before signalling at all.
    // The session's launch lock goes at the same moment: once the provider has
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

    let digest_summary = result.digest.as_ref().map(provider::Digest::summary);
    audit::log_result(
        &project_root,
        private,
        &audit_id,
        archetype,
        &result.provider,
        session_id,
        &stdin_instructions,
        &result.output,
        digest_summary.as_ref(),
        &result.leftovers,
    );
    // Any resume that actually *ran* refreshes the cold-cache clock, because the
    // clock tracks prompt-cache warmth, not answer quality. Reprocessing the
    // session prefix warms the cache regardless of whether the turn produced a
    // real final answer - a codex mid-turn death (which comes back as
    // `Ok`-with-a-death-digest) warmed it just as much as a clean run. Gating on
    // "did we get a good answer" (the old `!died_without_answer` clause) meant a
    // dead intermediate resume left the clock pinned to the last *successful*
    // touch, so the next resume of a genuinely-warm session was wrongly refused
    // as stale. `output.is_ok()` is the honest proxy for "reached the provider
    // and got output back"; a hard launch failure (`Err`, cache never warmed)
    // still doesn't refresh.
    let resume_ran = result.output.is_ok();
    if resume_ran {
        sessions::record(
            &project_root,
            private,
            &audit_id,
            archetype,
            &result.provider,
            session_id,
            "session",
            result.completed_epoch,
            // Recorded, not `None`: a resume now launches with an inherited
            // model and effort, and leaving them out of its own row would break
            // the chain at the first resume - the next one would find a row
            // naming neither and fall back to the provider default, which is
            // the bug this fixes, one link along.
            result.model.as_deref(),
            result.effort.as_deref(),
            Vec::new(),
            // The permissions this resume actually launched with - inherited
            // from the session's last recorded run, so the next resume in the
            // chain inherits from this row in turn.
            result.sandbox.as_deref(),
            result.writable_roots.clone(),
            &result.served,
            result.grok_trust.as_deref(),
            &stdin_instructions,
            &stdin_instructions,
            &result.output,
            digest_summary.as_ref(),
            &result.leftovers,
            result.codex_home.as_deref(),
        );
    }

    provider::print_result(&result);
    let left_running = print_left_running();
    println!("\nruntime: {}", format_runtime(started.elapsed()));

    // A stalled or dead resume comes back as `Ok` (so its session id, digest and
    // incident survive), but it is a failure for exit-code purposes - exiting 0
    // on a wedged run lies to scripts and CI, exactly as it did on the fan-out
    // path before this was fixed there. Anything left running outranks it.
    if let Some(code) = exit_status(run_failed(&result), left_running) {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_row(provider: &str) -> sessions::SessionRecord {
        record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": provider,
            "archetype": "security",
            "session_id": "s",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }))
    }

    #[test]
    fn a_resume_takes_the_provider_from_the_record() {
        // The session ID came from review's own output, whose run wrote this
        // row, so the provider is on record and is never asked for.
        let rec = resume_record("s", Some(minimal_row("codex"))).expect("resolved");
        assert_eq!(rec.provider, "codex");
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
                inherited_settings(Some(&rec), "grok").sandbox.as_deref(),
                Some(native)
            );
            rec.provider_sandbox = Some(native.to_string());
            rec.sandbox =
                crate::provider::recorded_sandbox("grok", Some(native)).map(str::to_string);
            assert_eq!(
                inherited_settings(Some(&rec), "grok").sandbox.as_deref(),
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
        let got = inherited_settings(Some(&rec), "codex");
        assert_eq!(got.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(got.writable_roots, vec!["/home/u/.brokkr", "/srv/cache"]);
        assert_eq!(got.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(got.effort.as_deref(), Some("low"));
    }

    #[test]
    fn a_row_recording_a_model_but_no_effort_inherits_just_the_model() {
        // Rows written before `effort` was recorded carry a model and nothing
        // else. Inheriting the model is still strictly better than falling back
        // to the provider's default model, so the absent effort must not
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
        let got = inherited_settings(Some(&rec), "codex");
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
        let got = inherited_settings(Some(&rec), "codex");
        assert_eq!(got.sandbox, None);
        assert!(got.writable_roots.is_empty());
        assert_eq!(got.model, None);
        assert_eq!(got.effort, None);
    }

    #[test]
    fn permissions_are_not_inherited_across_providers() {
        // `resume_record` takes the provider from the same row, so this cannot
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
        let got = inherited_settings(Some(&rec), "codex");
        assert_eq!(got.sandbox, None);
        assert!(got.writable_roots.is_empty());
        // The model is provider-scoped too: a grok model name handed to codex
        // is not a fallback, it is an invocation that cannot start.
        assert_eq!(got.model, None);
    }

    #[test]
    fn no_record_inherits_nothing() {
        let got = inherited_settings(None, "codex");
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
    fn a_recorded_provider_review_no_longer_supports_is_refused() {
        // A row naming a removed provider (kilo/opencode) must not turn into
        // an unrunnable invocation.
        let err = resume_error(Some(minimal_row("opencode")));
        assert!(err.contains("supported"), "{err}");
    }
}
