use anyhow::Result;

/// Token accounting summed across a run's `turn.completed` events (codex).
#[derive(Default, Clone)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

/// Structured summary of a codex run, distilled from its NDJSON stream plus the
/// `-o`/`--output-last-message` backstop. Grok and claude produce one from their
/// result object only when a run did not end in a clean answer.
pub struct Digest {
    /// Process exit code (`None` if terminated by a signal).
    pub exit_code: Option<i32>,
    /// Signal name when the process was killed by one, else `None`.
    pub signal: Option<String>,
    /// Whether the final message came from the authoritative `-o` file (which
    /// is written only on a real final message and survives a frozen stream).
    /// `false` means we fell back to the transcript or the last streamed
    /// message, or none at all.
    pub captured: bool,
    /// The `-o` file was empty but the real `final_answer` was salvaged from the
    /// on-disk rollout transcript (codex finished the turn but truncated its
    /// stream/`-o` on a shutdown-exit). The reported text is authentic, not an
    /// interim note.
    pub recovered_from_transcript: bool,
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
    /// Set when `review` killed codex rather than waiting for it to exit on its
    /// own: either the rollout watchdog fired (codex had written its final
    /// answer but would not exit) or the operator signalled us. The exit
    /// code/signal below will therefore describe *our* kill, not codex's own
    /// fate - without this field a watchdog kill would be indistinguishable
    /// from codex being killed by something else.
    pub terminated_by_review: Option<String>,
    /// How long the rollout had been unchanged when the watchdog decided to act.
    /// Recorded so a stall-timeout misfire on a healthy run shows how close the
    /// call was, rather than only that it happened.
    pub quiet_secs: Option<u64>,
    /// `type/payload.type` of the last rollout event - what codex was doing when
    /// it went silent.
    pub last_rollout_event: Option<String>,
    /// Message from a stream `error` / `turn.failed` event: codex itself
    /// explaining why the turn ended, most commonly an upstream refusal (content
    /// flagged, rate limit, auth). Distinct from every other failure mode here in
    /// that the cause is *known and stated*, so it must not be reported as a
    /// mystery death or retried - see `print_digest` and the auto-resume gate.
    pub turn_error: Option<String>,
    /// A failure diagnosed by review, independent of whether a provider
    /// produced an answer. Never a provider-stated reason or process status.
    pub review_error: Option<String>,
    /// The operator ended this run with `review interrupt`. Its missing answer
    /// is therefore deliberate: no auto-resume, no incident bundle, and the
    /// output says how to continue the session instead of reporting a death.
    pub interrupted: bool,
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
    /// An answer and successful verification are separate facts. Callers use
    /// this verdict for exit status, retaining captured for answer/retry logic.
    pub fn failed(&self) -> bool {
        self.review_error.is_some() || (!self.captured && !self.recovered_from_transcript)
    }

    pub fn summary(&self) -> DigestSummary {
        DigestSummary {
            exit_code: self.exit_code,
            signal: self.signal.clone(),
            captured: self.captured,
            recovered_from_transcript: self.recovered_from_transcript,
            turns: self.turns,
            task_complete: self.transcript.as_ref().map(|t| t.task_complete),
            stream_error: self.transcript.as_ref().map(|t| t.stream_error),
            input_tokens: self.usage.input_tokens,
            cached_input_tokens: self.usage.cached_input_tokens,
            output_tokens: self.usage.output_tokens,
            reasoning_output_tokens: self.usage.reasoning_output_tokens,
            incident_path: self.incident_path.clone(),
            terminated_by_review: self.terminated_by_review.clone(),
            quiet_secs: self.quiet_secs,
            last_rollout_event: self.last_rollout_event.clone(),
            turn_error: self.turn_error.clone(),
            review_error: self.review_error.clone(),
            interrupted: self.interrupted,
        }
    }
}

pub struct ProviderResult {
    pub provider: super::Provider,
    pub output: Result<String>,
    /// Session ID associated with this invocation, when one is known to the
    /// caller. Fresh runs capture the newly-created session; resumes retain
    /// the ID they were given.
    pub session_id: Option<String>,
    /// Resolved Codex home used for rollout lookup, persisted so resumes and
    /// session forensics can find the same session even when a profile set it.
    pub codex_home: Option<String>,
    /// Structured run summary (codex only, today).
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

/// A post-answer process problem is an observed status, not a provider-stated
/// turn failure. Both runners keep the answer and apply this same rule.
pub(super) fn answered_status_digest(status: Option<&std::process::ExitStatus>) -> Option<Digest> {
    (!status.is_some_and(std::process::ExitStatus::success)).then(|| Digest {
        captured: true,
        turn_error: None,
        ..no_answer_digest(String::new(), status)
    })
}

/// A grok or claude turn that ran but produced no answer, expressed the same way
/// codex's deaths are: `Ok` carrying a digest, never `Err`. Written for grok;
/// claude reports its turns the same way (a result object that says whether it
/// is an error), so everything below applies to it unchanged.
///
/// The distinction that matters is between *never reaching the provider* (a
/// spawn failure, an unknown model, a session that does not exist - nothing ran,
/// no session exists to resume, no prompt cache was warmed) and *running a turn
/// that produced no answer*. Only the first is an `Err`. Collapsing the second
/// into `Err` too costs two things that are invisible until you need them:
/// `invoke` forces `session_id: None` on the error path, so the id grok already
/// minted is dropped and the cut-off turn cannot be resumed - exactly the turn
/// most worth resuming; and `review resume` refreshes its cold-cache clock on
/// `output.is_ok()`, so a resume that ran and warmed the cache would leave the
/// clock stale and get the *next* resume refused. Codex avoids both by returning
/// `Ok` with a death digest, and grok has no reason to differ.
///
/// `turn_error` is the honest field for it: grok stated why the turn ended, so
/// this is a stated failure in exactly the sense codex means, and it prints
/// first, persists flat, and never invites a retry. The remaining fields
/// describe a run with no captured answer, which is what happened; the
/// codex-specific forensics stay `None` because grok has no rollout.
pub(super) fn no_answer_digest(
    reason: String,
    status: Option<&std::process::ExitStatus>,
) -> Digest {
    Digest {
        // The *observed* status, not an assumed one. Hardcoding `1` here made
        // the digest assert something it had never looked at - and the exit
        // code is the field an operator reads to tell "grok declined" from
        // "grok was killed". `None` when the keeper could not report it.
        exit_code: status.and_then(std::process::ExitStatus::code),
        signal: status.and_then(signal_name),
        captured: false,
        recovered_from_transcript: false,
        turns: 0,
        usage: Usage::default(),
        log_lines: Vec::new(),
        transcript: None,
        incident_path: None,
        terminated_by_review: None,
        quiet_secs: None,
        last_rollout_event: None,
        turn_error: Some(reason),
        review_error: None,
        interrupted: false,
    }
}

/// A missing provider result field is review's diagnosis, not a reason the
/// provider stated. Keep the no-answer and observed-status facts while
/// attributing the explanation to `review_error`.
pub(super) fn review_no_answer_digest(
    reason: String,
    status: Option<&std::process::ExitStatus>,
) -> Digest {
    let mut digest = no_answer_digest(String::new(), status);
    digest.turn_error = None;
    digest.review_error = Some(reason);
    digest
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
