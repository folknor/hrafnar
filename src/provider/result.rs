use anyhow::Result;

/// Token accounting summed across a run's `turn.completed` events (codex).
#[derive(Default, Clone)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

/// The forensic record of a run: process status, counts, and what codex left
/// on disk. How the run ended is not here - that is the `Outcome` on the
/// run's `Turn`, which every decision reads. Codex always builds one; grok and
/// claude only when the process status is worth keeping.
#[derive(Default)]
pub struct Digest {
    /// Process exit code (`None` if terminated by a signal or unknown).
    pub exit_code: Option<i32>,
    /// Signal name when the process was killed by one, else `None`.
    pub signal: Option<String>,
    pub turns: u32,
    pub usage: Usage,
    /// Non-JSON stdout lines (codex ERROR/WARN, apply_patch dumps). The harness
    /// can halt NDJSON emission on these, so we keep them visible.
    pub log_lines: Vec<String>,
    /// On-disk transcript forensics, read only when the run looks wrong
    /// (not captured, non-zero exit, or killed by a signal).
    pub transcript: Option<crate::transcript::TranscriptSummary>,
    /// Directory of the forensic bundle written for a suspicious run (stderr,
    /// raw stream, transcript tail, argv, codex version). `None` on clean runs.
    pub incident_path: Option<String>,
    /// How long the rollout had been unchanged when the watchdog decided to act.
    /// Recorded so a stall-timeout misfire on a healthy run shows how close the
    /// call was, rather than only that it happened.
    pub quiet_secs: Option<u64>,
    /// `type/payload.type` of the last rollout event - what codex was doing when
    /// it went silent.
    pub last_rollout_event: Option<String>,
}

/// Flat, serializable projection of a `Digest` for the sidecar and audit logs.
/// Flattened (no nested transcript) so failures are greppable after the fact -
/// e.g. `jq 'select(.digest.captured==false)'`. Without this, a codex run that
/// exits non-zero mid-shutdown is filed as a clean short `response` and the exit
/// status is lost. Absent for providers without a machine-readable stream.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct DigestSummary {
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub captured: bool,
    #[serde(default)]
    pub recovered_from_transcript: bool,
    pub turns: u32,
    /// From the rollout transcript (codex): the turn reached `task_complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_complete: Option<bool>,
    /// From the rollout transcript (codex): a `stream_error` froze the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_error: Option<bool>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    /// Forensic-bundle directory for a suspicious run (see `incident.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident_path: Option<String>,
    /// Why `review` killed codex, when it did. Persisted so the hang class is
    /// greppable after the fact:
    /// `jq 'select(.digest.terminated_by_review != null)'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminated_by_review: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rollout_event: Option<String>,
    /// Codex's own stated reason for the turn ending (stream `error` /
    /// `turn.failed`). Persisted so refusals are greppable and never confused
    /// with the hang class: `jq 'select(.digest.turn_error != null)'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_error: Option<String>,
    /// Review could not verify the run, even if it captured an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_error: Option<String>,
    /// Ended by `review interrupt`: `jq 'select(.digest.interrupted)'`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
}

impl Digest {
    /// The flat persisted form: these forensics plus the outcome's facts,
    /// under the field names the logs have always used.
    pub fn summary(&self, outcome: &super::Outcome) -> DigestSummary {
        use super::outcome::{Answer, EndedBy};
        DigestSummary {
            exit_code: self.exit_code,
            signal: self.signal.clone(),
            captured: outcome.answer == Answer::Captured,
            recovered_from_transcript: outcome.answer == Answer::Recovered,
            turns: self.turns,
            task_complete: self.transcript.as_ref().map(|t| t.task_complete),
            stream_error: self.transcript.as_ref().map(|t| t.stream_error),
            input_tokens: self.usage.input_tokens,
            cached_input_tokens: self.usage.cached_input_tokens,
            output_tokens: self.usage.output_tokens,
            reasoning_output_tokens: self.usage.reasoning_output_tokens,
            incident_path: self.incident_path.clone(),
            terminated_by_review: outcome.termination().map(str::to_string),
            quiet_secs: self.quiet_secs,
            last_rollout_event: self.last_rollout_event.clone(),
            turn_error: outcome.stated.clone(),
            review_error: outcome.unverified.clone(),
            interrupted: matches!(outcome.ended_by, EndedBy::Interrupted(_)),
        }
    }

    /// The facts the outcome lines are read against.
    pub fn status_facts(&self) -> super::outcome::StatusFacts<'_> {
        super::outcome::StatusFacts {
            exit_code: self.exit_code,
            signal: self.signal.as_deref(),
            quiet_secs: self.quiet_secs,
            last_event: self.last_rollout_event.as_deref(),
            task_complete: self.transcript.as_ref().map(|t| t.task_complete),
        }
    }
}

impl DigestSummary {
    pub fn status_facts(&self) -> super::outcome::StatusFacts<'_> {
        super::outcome::StatusFacts {
            exit_code: self.exit_code,
            signal: self.signal.as_deref(),
            quiet_secs: self.quiet_secs,
            last_event: self.last_rollout_event.as_deref(),
            task_complete: self.task_complete,
        }
    }
}

pub struct ProviderResult {
    pub provider: super::Provider,
    /// The turn the run established, or why none was: a spawn failure, an
    /// unknown model, a provider that ended before it showed the session
    /// began. Launch metadata below exists either way.
    pub output: Result<super::Turn>,
    /// Session ID associated with this invocation, when one is known to the
    /// caller. Fresh runs capture the newly-created session; resumes retain
    /// the ID they were given.
    pub session_id: Option<String>,
    /// Resolved Codex home used for rollout lookup, persisted so resumes and
    /// session forensics can find the same session even when a profile set it.
    pub codex_home: Option<String>,
    /// Forensics: always for codex, for grok and claude only when the process
    /// status is worth keeping.
    pub digest: Option<Digest>,
    /// UNIX seconds when this run actually finished. Stamped here rather than at
    /// sidecar-record time so the cold-cache clock reflects completion, not the
    /// (possibly much later) moment results are collected in launch order.
    pub completed_epoch: u64,
    /// The provider-specific permission decision for this invocation. Failed
    /// setup retains the request; successful setup carries its prepared selector. The sidecar normalizes known levels and saves
    /// this native selector separately for exact resume. Recorded because the
    /// guarantee a profile advertises is worth nothing if the permissions a run *received* are only
    /// visible in the launching terminal: `jq 'select(.sandbox=="read-only")'`
    /// has to be able to answer "which runs could not write?" after the fact.
    pub permissions: Option<super::Permissions>,
    /// Effective writable roots for this run beyond the provider's defaults
    /// (`src/writable_roots/`). Empty for every run that widened nothing,
    /// which is every read-only run.
    pub writable_roots: Vec<String>,
    /// The model and reasoning effort this run launched with, recorded for the
    /// same reason `permissions` is: they are properties of the *run*, and a
    /// `review resume` that cannot see them silently falls back to whatever
    /// the provider defaults to. `None` means "we passed nothing and the
    /// provider chose", which since `--ignore-user-config` means codex's
    /// built-in default rather than the operator's configured one.
    ///
    /// ModelSpec supplies the same selection to argv and these projections,
    /// including Codex raw selector precedence. These are launch selections,
    /// never a claim about which model the provider actually served.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// What the provider reports actually served the run. Record-only.
    pub served: Served,
    /// The folder grok was asked to trust permanently (`--trust`) for this run,
    /// when it had no trust decision yet. Recorded because the grant outlives
    /// the run and widens every later grok session there (the repo's `.grok`
    /// MCP/LSP config runs without a prompt) - a change to the operator's
    /// config that must be findable after the terminal is gone.
    pub grok_trust: Option<String>,
    /// What the run's processes left behind (see `contained`). Recorded in
    /// the audit and sidecar rows; never printed from here, since the closing
    /// report reads the process-wide ledger instead.
    pub leftovers: crate::contained::Leftovers,
}

/// What a provider reports, after the fact, about what served a run - as
/// opposed to `ProviderResult::model`, which is what we asked for.
///
/// The two are different names, not two spellings of one: grok's `-m grok-4.7`
/// is served as `grok-4.7-build`, and `-m grok-4.7-build` is refused as an
/// unknown model id. So this is recorded but **never** inherited by a
/// `review resume` - feeding it back to `-m` would fail the launch. Grok and
/// claude report it, from their result objects; empty for codex.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Served {
    /// Every model that served a call in the run, sorted and comma-joined (one
    /// in practice; a subagent on another model would add a second).
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
}

pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Internal result of one provider run before it's wrapped into a
/// `ProviderResult`. Lets the run helpers return the session ID and digest
/// alongside the text without a widening tuple.
pub struct RunOutput {
    pub text: String,
    pub outcome: super::Outcome,
    /// Session ID associated with this invocation, including a resumed ID.
    pub session_id: Option<String>,
    /// Resolved Codex home used for rollout lookup, when this is a Codex run.
    pub codex_home: Option<String>,
    pub digest: Option<Digest>,
    pub served: Served,
    /// What the run's processes left behind, across every invocation it made.
    pub leftovers: crate::contained::Leftovers,
}

/// Pull what served the run out of a grok or claude result object - both name
/// the fields `modelUsage` (keyed by model) and `total_cost_usd`. Parsed
/// separately from `interpret_grok_output` / `interpret_claude_output` because
/// it is record-only: nothing about whether the run answered depends on it, and
/// a result lacking these fields is still a valid result - it just records
/// nothing here.
pub(super) fn result_served(stdout: &str) -> Served {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(rename = "modelUsage", default)]
        model_usage: serde_json::Map<String, serde_json::Value>,
        #[serde(default)]
        total_cost_usd: Option<f64>,
    }
    let Ok(raw) = serde_json::from_str::<Raw>(stdout.trim()) else {
        return Served::default();
    };
    let names: Vec<&str> = raw.model_usage.keys().map(String::as_str).collect();
    Served {
        model: (!names.is_empty()).then(|| names.join(",")),
        cost_usd: raw.total_cost_usd,
    }
}

/// The process status of a grok or claude run, when it is worth keeping: a
/// non-zero or unknown exit after an answer is still an answer (the outcome
/// says so), but the status is the only sign something went wrong after the
/// turn, so it is recorded. `None` for a clean exit.
pub(super) fn answered_status_digest(status: Option<&std::process::ExitStatus>) -> Option<Digest> {
    (!status.is_some_and(std::process::ExitStatus::success)).then(|| status_digest(status))
}

/// A grok or claude run's observed process status as forensics. The *observed*
/// status, never an assumed one: the exit code is what an operator reads to
/// tell "the provider declined" from "the provider was killed", and `None`
/// means the keeper could not report it.
pub(super) fn status_digest(status: Option<&std::process::ExitStatus>) -> Digest {
    Digest {
        exit_code: status.and_then(std::process::ExitStatus::code),
        signal: status.and_then(signal_name),
        ..Digest::default()
    }
}

/// Signal name when a process was terminated by a signal, else `None`.
pub(super) fn signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|sig| match sig {
        2 => "SIGINT".to_string(),
        9 => "SIGKILL".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("signal {other}"),
    })
}

#[cfg(test)]
#[path = "result_tests.rs"]
mod tests;
