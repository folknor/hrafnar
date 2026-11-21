//! Fan-out invocations, automatic recovery, and recording.

use crate::{audit, provider, sessions};

/// The persisted summary of a run: its turn's outcome over its forensics,
/// absent only for a clean answer with nothing to record.
pub(super) fn summary_of(r: &provider::ProviderResult) -> Option<provider::DigestSummary> {
    r.output.as_ref().ok()?.summary(r.digest.as_ref())
}

/// The outcome of a run that established a turn.
fn outcome_of(r: &provider::ProviderResult) -> Option<&provider::Outcome> {
    r.output.as_ref().ok().map(|turn| &turn.outcome)
}

/// What one fan-out task needs to record its runs.
pub(super) struct RecordCtx {
    pub(super) project_root: std::path::PathBuf,
    pub(super) data: crate::data::DataDir,
    pub(super) scope: crate::data::LogScope,
    pub(super) archetype: String,
    pub(super) env_keys: Vec<String>,
    pub(super) profile_origin: Option<provider::ProfileOrigin>,
    pub(super) operator_prompt: String,
}

impl RecordCtx {
    pub(super) fn record(&self, result: &provider::ProviderResult, prompt: &str) {
        let _ = self.record_as(result, sessions::SessionKind::Run, prompt);
    }

    pub(super) fn record_as(
        &self,
        result: &provider::ProviderResult,
        kind: sessions::SessionKind,
        prompt: &str,
    ) -> bool {
        // Failed resumes did not warm the cache. This applies to automatic
        // and manual resumes alike; a requested ID is not proof a turn ran.
        // A run claude refused credentials for before doing anything never
        // reached the model either, though it comes back as a turn: a resume
        // must not refresh the cache clock, and a fresh session holding only
        // the refused prompt must not look resumable.
        let refused =
            outcome_of(result).is_some_and(|o| o.auth == Some(provider::AuthFailure::BeforeWork));
        let sidecar = !refused && (kind != sessions::SessionKind::Resume || result.output.is_ok());
        self.record_policy(result, kind, prompt, sidecar).1
    }

    fn record_policy(
        &self,
        result: &provider::ProviderResult,
        kind: sessions::SessionKind,
        prompt: &str,
        sidecar: bool,
    ) -> (bool, bool) {
        let row = sessions::SessionRow {
            timestamp: audit::chrono_utc(result.completed_epoch),
            epoch_secs: Some(result.completed_epoch),
            project: self.project_root.to_string_lossy().into_owned(),
            hostname: crate::config::hostname(),
            audit_id: self.scope.id().as_str().to_owned(),
            provider: result.provider.as_str().to_owned(),
            archetype: self.archetype.clone(),
            session_id: result.session_id.clone().unwrap_or_default(),
            kind,
            profile_origin: self.profile_origin.clone(),
            model: result.model.clone(),
            effort: result.effort.clone(),
            env_keys: self.env_keys.clone(),
            sandbox: result
                .permissions
                .as_ref()
                .and_then(provider::Permissions::recorded_level)
                .map(str::to_owned),
            provider_sandbox: result
                .permissions
                .as_ref()
                .and_then(provider::Permissions::native_to_record)
                .map(str::to_owned),
            writable_roots: result.writable_roots.clone(),
            served_model: result.served.model_label(),
            cost_usd: result.served.cost_usd,
            grok_trust: result.grok_trust.clone(),
            operator_prompt: self.operator_prompt.clone(),
            assembled_prompt: prompt.to_owned(),
            response: result.output.as_ref().ok().map(|t| t.text.clone()),
            error: result.output.as_ref().err().map(ToString::to_string),
            review_version: env!("CARGO_PKG_VERSION").to_owned(),
            digest: summary_of(result),
            leftovers: (!result.leftovers.is_empty()).then(|| result.leftovers.clone()),
            codex_home: result.codex_home.clone(),
            // Only a token that was accepted: on a `run` row this becomes the
            // session's credential policy, and a rejected token must not make
            // later messages require it.
            credential: result
                .credential
                // An answer is the evidence: an interrupted or dead attempt
                // with no auth error never showed the token worked.
                .filter(|_| outcome_of(result).is_some_and(|o| o.auth.is_none() && o.has_answer()))
                .map(str::to_owned),
        };
        let audit_written = audit::log_result(&self.data, &self.scope, &row);
        // A manual failed launch did not warm the cache. Automatic recovery is
        // already an established invocation and retains its historical row policy.
        let sidecar_written = sidecar
            && result.session_id.is_some()
            && sessions::append(&self.data, &self.scope, &row);
        (audit_written, sidecar_written)
    }

    fn record_task(&self, outcome: &TaskOutcome) {
        let pending = &outcome.invocations[outcome.prerecorded..];
        if pending.is_empty() {
            return;
        }
        let mut recorded = false;
        for invocation in pending {
            recorded = self.record_as(
                &invocation.result,
                invocation.kind.clone(),
                &invocation.prompt,
            );
        }
        let final_run = &pending.last().expect("nonempty").result;
        for invocation in pending {
            let _ = self.publish(&invocation.result, final_run, recorded);
        }
    }

    /// Record an attempt that is being abandoned for a replacement: audit only,
    /// never the session sidecar (it advanced no session), and publish its own
    /// receipt now, naming itself with no sidecar written, so anything waiting
    /// on its id settles instead of waiting on an unrelated replacement. This
    /// is where its id stops being a target for `review interrupt`. False when
    /// its audit row could not be written or its receipt could not be
    /// published, in which case the replacement must not launch: the
    /// abandoned attempt would be unaccounted for, or its waiters stuck.
    fn settle_abandoned(&self, result: &provider::ProviderResult, prompt: &str) -> bool {
        let (audited, _) = self.record_policy(result, sessions::SessionKind::Run, prompt, false);
        let published = self.publish(result, result, false);
        audited && published
    }

    pub(super) fn record_message(&self, result: &provider::ProviderResult, prompt: &str) -> bool {
        // A launch failure never warmed the cache, so it must not become the
        // latest row used by the age gate or inheritance.
        self.record_as(result, sessions::SessionKind::Resume, prompt)
    }
    pub(super) fn publish(
        &self,
        requested: &provider::ProviderResult,
        final_run: &provider::ProviderResult,
        sidecar_written: bool,
    ) -> bool {
        // No run id: no marker was ever published, so nothing waits on one.
        let Some(id) = &requested.run_id else {
            return true;
        };
        let handoff = sessions::handoff::Handoff {
            schema: sessions::handoff::PROTOCOL,
            requested: id.clone(),
            interrupted: outcome_of(requested).is_some_and(provider::Outcome::interrupted),
            final_session: final_run.session_id.clone(),
            sidecar_written,
        };
        match sessions::handoff::publish(&self.data, &handoff) {
            Ok(()) => {
                if let Some(reservation) = &requested._reservation {
                    reservation.complete();
                }
                true
            }
            Err(e) => {
                eprintln!("warning: failed to publish run handoff: {e}");
                false
            }
        }
    }
}

/// Scheduling and execution context around one complete launch request.
pub(super) struct Launch {
    pub(super) spec: provider::LaunchSpec,
    pub(super) runtime: provider::ProviderRuntime,
    pub(super) delay: std::time::Duration,
}

impl Launch {
    async fn invoke(
        &self,
        start: provider::SessionStart,
        prompt: &str,
        predecessor: Option<crate::sessions::handoff::RunId>,
    ) -> provider::ProviderResult {
        let spec = provider::LaunchSpec {
            start,
            prompt: prompt.to_owned(),
            ..self.spec.clone()
        };
        // Fan-out stagger and internal retries do not need a launch handshake.
        provider::invoke(spec, None, &self.runtime, predecessor).await
    }
}

/// One completed invocation and the exact prompt submitted to it. Recording
/// order is independent of which invocation supplies the displayed result.
struct RecordedInvocation {
    result: provider::ProviderResult,
    kind: sessions::SessionKind,
    prompt: String,
}

pub(super) struct TaskOutcome {
    invocations: Vec<RecordedInvocation>,
    selected: usize,
    /// Leading invocations already recorded and published before the task
    /// went on (an abandoned attempt settled before its replacement launched).
    prerecorded: usize,
}

impl TaskOutcome {
    fn single(result: provider::ProviderResult, prompt: String) -> Self {
        Self {
            invocations: vec![RecordedInvocation {
                result,
                kind: sessions::SessionKind::Run,
                prompt,
            }],
            selected: 0,
            prerecorded: 0,
        }
    }
    fn result(&self) -> &provider::ProviderResult {
        &self.invocations[self.selected].result
    }
    pub(super) fn into_result(mut self) -> provider::ProviderResult {
        self.invocations.swap_remove(self.selected).result
    }
}

/// What to do about a fresh run that has just ended.
#[derive(Debug, PartialEq)]
enum AfterRun {
    /// Report it as it is.
    Report,
    /// The operator interrupted it; resuming past that would undo exactly what
    /// they asked for.
    Interrupted,
    /// It stated why it produced nothing; a retry would buy the same refusal.
    StatedFailure(String),
    /// Its teardown left processes running, or could not say: a resume would
    /// run beside processes that may still hold the session or edit the tree.
    LeftRunning,
    /// A death its provider recovers from (`provider::recovery`): resume this
    /// session once with the given nudge, cache still warm.
    AutoResume {
        session: String,
        nudge: &'static str,
    },
}

/// Whether to auto-resume a fresh run that has just ended, on the same
/// profile. Whether an outcome is a recoverable death at all is the provider
/// adapter's reading (`provider::recovery`; today only codex's mid-turn wedge
/// qualifies). The vetoes here hold for every provider and are applied first:
/// an interrupted run, a stated reason for producing nothing, and a teardown
/// that left processes running are left alone.
fn after_run(first: &provider::ProviderResult) -> AfterRun {
    let outcome = outcome_of(first);
    if outcome.is_some_and(provider::Outcome::interrupted) {
        return AfterRun::Interrupted;
    }
    // A stated reason for producing nothing is a verdict on the request
    // itself: resending it buys the identical failure at full cost, and files
    // a second incident bundle that looks like a second death.
    if let Some(why) = outcome.and_then(provider::Outcome::stated_failure) {
        return AfterRun::StatedFailure(why.to_string());
    }
    let Some(nudge) = provider::recovery(first) else {
        return AfterRun::Report;
    };
    // Only a run its provider would resume has a resume to withhold.
    if !first.leftovers.is_clean() {
        return AfterRun::LeftRunning;
    }
    match &first.session_id {
        Some(sid) if provider::SessionId::parse(sid).is_ok() => AfterRun::AutoResume {
            session: sid.clone(),
            nudge,
        },
        Some(_) | None => AfterRun::Report,
    }
}

/// Which of a dead run and its auto-resume to report. The resume wins when it
/// produced an answer, and also when the operator interrupted it: then it is
/// the run the interrupt is about, and reporting the first run's death instead
/// would hide the resume command.
fn after_auto_resume(
    first: provider::ProviderResult,
    mut second: provider::ProviderResult,
    first_prompt: String,
    second_prompt: String,
) -> TaskOutcome {
    let answered = outcome_of(&second).is_some_and(provider::Outcome::has_answer);
    let selected =
        usize::from(answered || outcome_of(&second).is_some_and(provider::Outcome::interrupted));
    if answered && let Ok(turn) = second.output.as_mut() {
        // Existing application annotation is also persisted in the response;
        // it is independent of the actual submitted prompt retained below.
        turn.text = format!(
            "(auto-resumed after the initial run died without a final answer)\n\n{}",
            turn.text
        );
    }
    TaskOutcome {
        invocations: vec![
            RecordedInvocation {
                result: first,
                kind: sessions::SessionKind::Run,
                prompt: first_prompt,
            },
            RecordedInvocation {
                result: second,
                kind: sessions::SessionKind::Resume,
                prompt: second_prompt,
            },
        ],
        selected,
        prerecorded: 0,
    }
}

/// A fresh claude run that failed to authenticate before doing anything is
/// launched once more as a new session, with a token from the global
/// `claude_token_command`. `Err` hands the first result back untouched when
/// that does not apply; otherwise the first attempt is already settled.
///
/// Only a failure the stream shows came before any model output or tool
/// activity, in a fully observed stream, is resent: anything else may have
/// done work a resend would repeat. Not when the child already had a token
/// (inherited or from the profile): that is the credential that was refused.
/// Not after a stop. Startup hooks run again, as on any relaunch. The first
/// attempt is settled - audited, kept out of the sidecar, its receipt
/// published - before the command runs, so its id stops being a target for
/// `review interrupt` there; a receipt that cannot be published stops the
/// retry, since waiters on that id would otherwise hang behind it.
async fn claude_auth_retry(
    launch: &Launch,
    ctx: &RecordCtx,
    first: provider::ProviderResult,
) -> Result<TaskOutcome, Box<provider::ProviderResult>> {
    let Some(auth) = outcome_of(&first).and_then(|o| o.auth) else {
        return Err(Box::new(first));
    };
    let hint = if launch.runtime.claude_token.is_none() {
        Some(
            "claude could not authenticate. If claude is logged in through \
             CLAUDE_CODE_OAUTH_TOKEN in your login shell: Claude Code does not pass that \
             variable to the commands it runs, so a review started from inside a Claude Code \
             session lacks it. Launch from a terminal, or set [_defaults].claude_token_command \
             in the global review config (`review man config`).",
        )
    } else if launch.spec.settings.has_own_claude_token() {
        Some(
            "claude refused the CLAUDE_CODE_OAUTH_TOKEN it was given; not retrying with claude_token_command.",
        )
    } else if auth != provider::AuthFailure::BeforeWork {
        Some(
            "claude failed to authenticate after it may have done work, or its output was not \
             fully read; not retrying, since a resend could repeat that work.",
        )
    } else if !first.leftovers.is_clean() {
        // A startup hook can leave something running even when the model was
        // never reached; a replacement must not run beside it.
        Some(
            "not retrying: processes the failed attempt started survived teardown, or its \
             teardown could not be confirmed.",
        )
    } else if launch.runtime.lifecycle.stop_requested() {
        Some("review is stopping; not retrying.")
    } else {
        None
    };
    if let Some(hint) = hint {
        eprintln!("{hint}");
        return Err(Box::new(first));
    }
    let settled = |first| TaskOutcome {
        invocations: vec![RecordedInvocation {
            result: first,
            kind: sessions::SessionKind::Run,
            prompt: launch.spec.prompt.clone(),
        }],
        selected: 0,
        prerecorded: 1,
    };
    // Audited either way; only a published receipt lets the retry go on.
    if !ctx.settle_abandoned(&first, &launch.spec.prompt) {
        eprintln!(
            "not retrying: the failed attempt could not be audited, or its handoff could not be published"
        );
        return Ok(settled(first));
    }
    let token = match launch.runtime.fetch_claude_token().await {
        Some(Ok(token)) => token,
        Some(Err(why)) => {
            eprintln!(
                "claude could not authenticate, and claude_token_command gave no token: {why}"
            );
            return Ok(settled(first));
        }
        None => return Ok(settled(first)),
    };
    if launch.runtime.lifecycle.stop_requested() {
        eprintln!("review is stopping; not retrying.");
        return Ok(settled(first));
    }
    eprintln!(
        "claude could not authenticate; retrying once as a new session with a token from claude_token_command"
    );
    let spec = provider::LaunchSpec {
        start: provider::SessionStart::Fresh { id: None },
        settings: launch.spec.settings.clone().with_claude_token(token),
        ..launch.spec.clone()
    };
    // A launch of its own, so it takes the host's launch lock like any other
    // and releases it once spawned (the fan-out's may be long gone).
    let lock = tokio::task::spawn_blocking(|| {
        let file = crate::lock::open_lock_file(&std::env::temp_dir().join("review.lock"))?;
        crate::lock::acquire_blocking(&file)?;
        Ok::<_, anyhow::Error>(file)
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|r| r);
    let lock = match lock {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("not retrying: could not take the launch lock: {e}");
            return Ok(settled(first));
        }
    };
    let (launched_tx, launched_rx) = tokio::sync::oneshot::channel();
    let invoke = provider::invoke(
        spec,
        Some(launched_tx),
        &launch.runtime,
        first.run_id.clone(),
    );
    tokio::pin!(invoke);
    let second = tokio::select! {
        result = &mut invoke => {
            drop(lock);
            result
        }
        _ = launched_rx => {
            drop(lock);
            invoke.await
        }
    };
    Ok(TaskOutcome {
        invocations: vec![
            RecordedInvocation {
                result: first,
                kind: sessions::SessionKind::Run,
                prompt: launch.spec.prompt.clone(),
            },
            RecordedInvocation {
                result: second,
                kind: sessions::SessionKind::Run,
                prompt: launch.spec.prompt.clone(),
            },
        ],
        selected: 1,
        prerecorded: 1,
    })
}

/// One fan-out task: launch, maybe auto-resume, then record.
///
/// Recorded within each fan-out task, after any recovery, rather than after
/// every sibling task has finished: `review resume` needs the sidecar row, and `review interrupt` waits for
/// it, so a run must not stay unresumable until its slowest sibling finishes.
/// Rows are written in launch order, so the most recent invocation also wins a
/// same-second timestamp tie.
pub(super) async fn run_launch(launch: Launch, ctx: std::sync::Arc<RecordCtx>) -> TaskOutcome {
    // Held until the rows below are written, so a signalled `review` does not
    // exit with this session unrecorded (see `supervisor::install_signal_supervisor`).
    let _unrecorded = launch
        .runtime
        .lifecycle
        .unrecorded_for(launch.spec.provider);
    if !launch.delay.is_zero() {
        tokio::time::sleep(launch.delay).await;
    }
    let prov = launch.spec.provider;
    let first = launch
        .invoke(
            provider::SessionStart::Fresh { id: None },
            &launch.spec.prompt,
            None,
        )
        .await;
    let outcome = match after_run(&first) {
        AfterRun::Report => TaskOutcome::single(first, launch.spec.prompt.clone()),
        AfterRun::Interrupted => {
            eprintln!("{prov} run interrupted - not auto-resuming");
            TaskOutcome::single(first, launch.spec.prompt.clone())
        }
        AfterRun::LeftRunning => {
            eprintln!(
                "{prov} run died without a final answer; not auto-resuming - processes \
                 it started survived teardown, or its teardown could not be confirmed"
            );
            TaskOutcome::single(first, launch.spec.prompt.clone())
        }
        AfterRun::StatedFailure(why) => {
            eprintln!("{prov} ended the turn without a final answer: {why}");
            match claude_auth_retry(&launch, &ctx, first).await {
                Ok(outcome) => outcome,
                Err(first) => {
                    eprintln!(
                        "not auto-resuming - {prov} stated a reason, so a retry would hit it again"
                    );
                    TaskOutcome::single(*first, launch.spec.prompt.clone())
                }
            }
        }
        AfterRun::AutoResume {
            session: sid,
            nudge,
        } => {
            eprintln!("{prov} session {sid} died without a final answer - auto-resuming once");
            let second = launch
                .invoke(
                    provider::SessionStart::Resume(
                        provider::SessionId::parse(&sid)
                            .expect("auto-resume identity validated before selection"),
                    ),
                    nudge,
                    first.run_id.clone(),
                )
                .await;
            after_auto_resume(first, second, launch.spec.prompt.clone(), nudge.to_string())
        }
    };
    // Both runs are recorded only now, after any auto-resume. Keep paired rows
    // in launch order so the resume remains latest when their epoch seconds tie.
    // Pending markers stay discoverable across the recovery/recording gap.
    // The second marker names its direct predecessor so new messages can
    // target recovery, while existing waiters retain the original run ID.
    ctx.record_task(&outcome);
    // During graceful stop, the supervisor waits for interruptible turns'
    // recording guards, then may exit before normal result printing. Emit this
    // interrupted session's continuation hint here. This notification snapshot
    // controls the hint only; it is not an admission check.
    if launch.runtime.lifecycle.stop_requested()
        && let (Some(sid), Some(true)) = (
            outcome.result().session_id.as_deref(),
            outcome_of(outcome.result()).map(provider::Outcome::interrupted),
        )
    {
        eprintln!("{}", provider::resume_hint(prov, sid));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contained;

    /// A codex result with session `sid`: `answered` says whether `-o` captured
    /// a final answer; `interrupted` whether `review interrupt` ended it.
    fn codex_run(sid: &str, answered: bool, interrupted: bool) -> provider::ProviderResult {
        let answer = if answered {
            provider::Answer::Captured
        } else {
            provider::Answer::None
        };
        provider::ProviderResult {
            credential: None,
            run_id: None,
            _reservation: None,
            provider: provider::Provider::Codex,
            output: Ok(provider::Turn {
                text: format!("text of {sid}"),
                outcome: provider::Outcome::answered(answer).ended(interrupted, false, None),
            }),
            session_id: Some(sid.into()),
            codex_home: None,
            digest: Some(provider::Digest {
                exit: crate::process::Exit::Code(i32::from(!answered)),
                turns: 1,
                ..provider::Digest::default()
            }),
            completed_epoch: 0,
            permissions: None,
            writable_roots: Vec::new(),
            model: None,
            effort: None,
            served: provider::Served::default(),
            grok_trust: None,
            leftovers: contained::Leftovers::default(),
        }
    }

    fn auto_resume(sid: &str) -> AfterRun {
        let nudge = provider::recovery(&codex_run(sid, false, false)).expect("codex recovers");
        AfterRun::AutoResume {
            session: sid.into(),
            nudge,
        }
    }

    #[test]
    fn recording_keeps_completion_time_and_failed_recovery_out_of_cache_history() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        let data = crate::data::DataDir::at(root.clone());
        let scope = crate::data::LogScope::new(
            crate::data::AuditId::from_record("audit").expect("id"),
            false,
        );
        let ctx = RecordCtx {
            project_root: root.join("project"),
            data: data.clone(),
            scope: scope.clone(),
            archetype: "bare".into(),
            env_keys: Vec::new(),
            profile_origin: None,
            operator_prompt: "operator".into(),
        };
        let mut first = codex_run("session", false, false);
        first.run_id = Some(crate::sessions::handoff::RunId::default());
        first.completed_epoch = 42;
        let mut second = codex_run("session", false, false);
        second.run_id = Some(crate::sessions::handoff::RunId::default());
        second.output = Err(anyhow::anyhow!("failed launch"));
        second.completed_epoch = 43;
        let outcome = after_auto_resume(first, second, "original".into(), "nudge".into());
        let requested = outcome.invocations[0].result.run_id.as_ref().expect("run");
        assert!(
            sessions::handoff::read(&data, requested)
                .expect("receipt")
                .is_none()
        );
        ctx.record_task(&outcome);
        let rows = sessions::read_all_in(&data);
        assert_eq!(rows.len(), 1, "failed recovery must not refresh the cache");
        assert_eq!(rows[0].epoch_secs, Some(42));
        let audit_path = data
            .audit(&ctx.project_root, &scope)
            .expect("audit path")
            .join("audit.jsonl");
        let audit = std::fs::read_to_string(audit_path).expect("audit");
        let audit_rows: Vec<serde_json::Value> = audit
            .lines()
            .map(|l| serde_json::from_str(l).expect("audit row"))
            .collect();
        assert_eq!(audit_rows.len(), 2);
        assert_eq!(audit_rows[0]["timestamp"], rows[0].timestamp);
        assert_eq!(audit_rows[1]["session"], "session");
        let handoff = sessions::handoff::read(&data, requested)
            .expect("read")
            .expect("published");
        assert!(
            !handoff.interrupted && !handoff.sidecar_written,
            "a prior successful append cannot substitute for final handoff"
        );
        assert_eq!(handoff.final_session.as_deref(), Some("session"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recording_routes_both_families_through_one_injected_domain() {
        use crate::data::{AuditId, DataDir, LogFamily, LogScope};
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        let data = DataDir::at(root.clone());
        let project = root.join("project");
        for (private, sid) in [(false, "public"), (true, "private")] {
            let scope = LogScope::new(AuditId::from_record("beef").expect("test fixture"), private);
            let ctx = RecordCtx {
                project_root: project.clone(),
                data: data.clone(),
                scope: scope.clone(),
                archetype: "bare".into(),
                env_keys: Vec::new(),
                profile_origin: None,
                operator_prompt: "operator".into(),
            };
            let result = codex_run(sid, true, false);
            ctx.record(&result, "assembled");
            ctx.record_as(&result, sessions::SessionKind::Resume, "followup");
            let rows =
                std::fs::read_to_string(data.sessions(scope.family()).expect("test fixture"))
                    .expect("test fixture");
            let rows: Vec<sessions::SessionRecord> = rows
                .lines()
                .map(|line| serde_json::from_str(line).expect("test fixture"))
                .collect();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].session_id, sid);
            assert_eq!(rows[0].audit_id, "beef");
            assert_eq!(rows[0].kind, sessions::SessionKind::Run);
            assert_eq!(rows[1].kind, sessions::SessionKind::Resume);
            assert_eq!(rows[1].assembled_prompt, "followup");
            let audit = std::fs::read_to_string(
                data.audit(&project, &scope)
                    .expect("test fixture")
                    .join("audit.jsonl"),
            )
            .expect("test fixture");
            assert_eq!(audit.lines().count(), 2);
            let runtime = provider::ProviderRuntime::from_config(
                crate::supervisor::Lifecycle::default(),
                None,
                None,
                data.clone(),
                &scope,
            );
            assert_eq!(
                runtime.data.orphan_log(runtime.log_family),
                Some(root.join(if private {
                    "review/teardowns-private.jsonl"
                } else {
                    "review/teardowns.jsonl"
                }))
            );
        }
        assert_eq!(sessions::read_all_in(&data).len(), 4);
        assert!(
            data.sessions(LogFamily::Public)
                .expect("test fixture")
                .exists()
        );
        assert!(
            data.sessions(LogFamily::Private)
                .expect("test fixture")
                .exists()
        );
        std::fs::remove_dir_all(root).expect("test fixture");
    }

    #[test]
    fn recovery_invocations_keep_prompts_in_chronological_order() {
        for (answered, interrupted, selected) in
            [(true, false, 1), (false, true, 1), (false, false, 0)]
        {
            let outcome = after_auto_resume(
                codex_run("s", false, false),
                codex_run("s", answered, interrupted),
                "assembled".into(),
                "actual nudge".into(),
            );
            assert_eq!(outcome.selected, selected);
            assert_eq!(outcome.invocations[0].prompt, "assembled");
            assert_eq!(outcome.invocations[1].prompt, "actual nudge");
            assert_eq!(outcome.invocations[0].kind, sessions::SessionKind::Run);
            assert_eq!(outcome.invocations[1].kind, sessions::SessionKind::Resume);
        }
    }

    #[test]
    fn only_a_provider_that_recovers_is_auto_resumed() {
        // The same dead outcome on claude is reported as it is: claude has no
        // mid-turn wedge for a resume to finish.
        let mut claude = codex_run("s", false, false);
        claude.provider = provider::Provider::Claude;
        assert_eq!(after_run(&claude), AfterRun::Report);
        // And a claude death with leftovers has no resume to withhold.
        claude.leftovers.unknown = Some("keeper died".into());
        assert_eq!(after_run(&claude), AfterRun::Report);
    }

    #[test]
    fn a_codex_death_is_auto_resumed() {
        assert_eq!(after_run(&codex_run("s", false, false)), auto_resume("s"));
    }

    #[test]
    fn a_death_that_left_processes_running_is_not_auto_resumed() {
        let mut dead = codex_run("s", false, false);
        dead.leftovers.survivors.push(crate::keeper::Proc {
            pid: 1,
            cmdline: "python3 loop.py".into(),
            truncated: false,
        });
        assert_eq!(after_run(&dead), AfterRun::LeftRunning);
        let mut unknown = codex_run("s", false, false);
        unknown.leftovers.unknown = Some("keeper died".into());
        assert_eq!(after_run(&unknown), AfterRun::LeftRunning);
        // Cleanly killed leftovers change nothing.
        let mut killed = codex_run("s", false, false);
        killed.leftovers.killed.push(crate::keeper::Proc {
            pid: 1,
            cmdline: "sleep 600".into(),
            truncated: false,
        });
        assert_eq!(after_run(&killed), auto_resume("s"));
    }

    #[test]
    fn an_interrupted_run_is_not_auto_resumed() {
        assert_eq!(
            after_run(&codex_run("s", false, true)),
            AfterRun::Interrupted
        );
    }

    #[test]
    fn an_answered_run_is_reported() {
        assert_eq!(after_run(&codex_run("s", true, false)), AfterRun::Report);
    }

    #[test]
    fn an_interrupted_auto_resume_is_the_run_reported() {
        // Reporting the first run's death instead would hide the resume
        // command, and its row must come first so the resume is the latest.
        let outcome = after_auto_resume(
            codex_run("first", false, false),
            codex_run("second", false, true),
            "initial".into(),
            "nudge".into(),
        );
        assert_eq!(
            outcome
                .result()
                .output
                .as_ref()
                .ok()
                .map(|t| t.text.as_str()),
            Some("text of second")
        );
        assert!(
            outcome.invocations[0]
                .result
                .output
                .as_ref()
                .is_ok_and(|t| t.text == "text of first")
        );
    }

    #[test]
    fn a_failed_auto_resume_keeps_the_death_as_the_result() {
        let outcome = after_auto_resume(
            codex_run("first", false, false),
            codex_run("second", false, false),
            "initial".into(),
            "nudge".into(),
        );
        assert_eq!(
            outcome
                .result()
                .output
                .as_ref()
                .ok()
                .map(|t| t.text.as_str()),
            Some("text of first")
        );
    }

    #[test]
    fn a_rescuing_auto_resume_is_reported_and_labelled() {
        let outcome = after_auto_resume(
            codex_run("first", false, false),
            codex_run("second", true, false),
            "initial".into(),
            "nudge".into(),
        );
        let text = outcome.into_result().output.expect("text").text;
        assert!(text.starts_with("(auto-resumed"), "got {text:?}");
        assert!(text.ends_with("text of second"));
    }
}
