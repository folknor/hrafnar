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

/// Write one provider run to the audit log and, if it captured a session, the
/// sidecar. Shared so an auto-resume can persist *both* of its invocations.
#[allow(clippy::too_many_arguments)]
fn record_run(
    project_root: &std::path::Path,
    private: bool,
    audit_id: &str,
    archetype: &str,
    env_keys: &[String],
    profile_origin: Option<&provider::ProfileOrigin>,
    operator_prompt: &str,
    prompt: &str,
    result: &provider::ProviderResult,
    kind: sessions::SessionKind,
) {
    let session_for_log = result.session_id.as_deref().unwrap_or("");
    let digest_summary = summary_of(result);
    audit::log_result(
        project_root,
        private,
        audit_id,
        archetype,
        result.provider.as_str(),
        session_for_log,
        prompt,
        &result.output,
        digest_summary.as_ref(),
        &result.leftovers,
    );
    if let Some(ref sid) = result.session_id {
        sessions::record_with_kind(
            project_root,
            private,
            audit_id,
            archetype,
            result.provider,
            sid,
            kind,
            result.completed_epoch,
            // From the result, not from the caller's profile: the recorded
            // model must be the one the run launched with, for the same reason
            // the sandbox level is - a resume inherits this row, and inheriting
            // a request rather than an outcome is what this fix is about.
            result.model.as_deref(),
            result.effort.as_deref(),
            env_keys.to_vec(),
            result.permissions.as_ref(),
            result.writable_roots.clone(),
            &result.served,
            result.grok_trust.as_deref(),
            operator_prompt,
            prompt,
            &result.output,
            digest_summary.as_ref(),
            &result.leftovers,
            result.codex_home.as_deref(),
            profile_origin,
        );
    }
}

/// What one fan-out task needs to record its runs.
pub(super) struct RecordCtx {
    pub(super) project_root: std::path::PathBuf,
    pub(super) private: bool,
    pub(super) audit_id: String,
    pub(super) archetype: String,
    pub(super) env_keys: Vec<String>,
    pub(super) profile_origin: Option<provider::ProfileOrigin>,
    pub(super) operator_prompt: String,
}

impl RecordCtx {
    pub(super) fn record(&self, result: &provider::ProviderResult, prompt: &str) {
        self.record_as(result, sessions::SessionKind::Run, prompt);
    }

    fn record_as(
        &self,
        result: &provider::ProviderResult,
        kind: sessions::SessionKind,
        prompt: &str,
    ) {
        record_run(
            &self.project_root,
            self.private,
            &self.audit_id,
            &self.archetype,
            &self.env_keys,
            self.profile_origin.as_ref(),
            &self.operator_prompt,
            prompt,
            result,
            kind,
        );
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
    ) -> provider::ProviderResult {
        let spec = provider::LaunchSpec {
            start,
            prompt: prompt.to_owned(),
            ..self.spec.clone()
        };
        // Fan-out stagger and internal retries do not need a launch handshake.
        provider::invoke(spec, None, &self.runtime).await
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
    }
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
            eprintln!("not auto-resuming - {prov} stated a reason, so a retry would hit it again");
            TaskOutcome::single(first, launch.spec.prompt.clone())
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
                )
                .await;
            after_auto_resume(first, second, launch.spec.prompt.clone(), nudge.to_string())
        }
    };
    // Both runs are recorded only now, after any auto-resume. Keep paired rows
    // in launch order so the resume remains latest when their epoch seconds tie.
    // Between the first run ending and the auto-resume spawning, the session
    // has no in-flight marker; it has no sidecar row either, so a message
    // arriving then is refused ("no record") instead of launching. Recording
    // the first run before auto-resuming would give it a row with no marker,
    // and the message would start a second turn on the session. A message
    // arriving while the auto-resume runs finds its marker (written before its
    // spawn) and interrupts it, as for any run.
    for invocation in &outcome.invocations {
        ctx.record_as(
            &invocation.result,
            invocation.kind.clone(),
            &invocation.prompt,
        );
    }
    // A signalled `review` exits once every codex run is recorded, before the
    // results are printed, so say here how to continue this one.
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
            provider: provider::Provider::Codex,
            output: Ok(provider::Turn {
                text: format!("text of {sid}"),
                outcome: provider::Outcome::answered(answer).ended(interrupted, false, None),
            }),
            session_id: Some(sid.into()),
            codex_home: None,
            digest: Some(provider::Digest {
                exit_code: Some(i32::from(!answered)),
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
