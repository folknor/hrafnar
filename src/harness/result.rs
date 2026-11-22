//! What a harness invocation returns. `HarnessResult` carries the turn (or
//! the launch error), the session id, the `Digest` and the permissions, model
//! and effort the run actually launched with. `Digest` holds the facts common
//! to every harness (process exit, turns, usage, incident path) plus optional
//! `HarnessForensics`; `DigestSummary` is its flat, serializable projection,
//! persisted into the audit and session rows so failures stay greppable.

pub use super::codex::forensics::CodexForensics;
use anyhow::Result;

/// Token accounting for a run, in codex's convention: `total_input` counts
/// every input token, the cached ones included, and `cached_input` is the
/// cached part of it. Harnesses that report cache reads (and cache writes)
/// separately go through `from_parts`, the one place that convention is
/// rebuilt, so no mapper can double-count or drop a part.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub total_input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning_output: u64,
}

impl Usage {
    /// Build from separately reported parts: input that hit no cache, input
    /// written to the cache, and input read from it. A harness that reports
    /// no cache writes passes zero.
    pub fn from_parts(
        uncached_input: u64,
        cache_creation: u64,
        cache_read: u64,
        output: u64,
        reasoning_output: u64,
    ) -> Self {
        Self {
            total_input: uncached_input + cache_creation + cache_read,
            cached_input: cache_read,
            output,
            reasoning_output,
        }
    }
}

impl std::ops::AddAssign<&Usage> for Usage {
    fn add_assign(&mut self, other: &Usage) {
        self.total_input += other.total_input;
        self.cached_input += other.cached_input;
        self.output += other.output;
        self.reasoning_output += other.reasoning_output;
    }
}

/// The forensic record of a run: process status, counts, and what codex left
/// on disk. How the run ended is not here - that is the `Outcome` on the
/// run's `Turn`, which every decision reads. Codex always builds one; grok and
/// claude only when the process status is worth keeping.
#[derive(Default)]
pub struct Digest {
    /// How the harness process ended.
    pub exit: crate::process::Exit,
    /// The harness's own count, in its own unit: codex counts
    /// `turn.completed` events, claude and grok report `num_turns` (API round
    /// trips). Not comparable across harnesses, and not normalised: no
    /// harness reports the other's unit.
    pub turns: u32,
    pub usage: Usage,
    /// Directory of the forensic bundle written for a suspicious run (stderr,
    /// raw stream, transcript tail, argv, codex version). `None` on clean runs.
    pub incident_path: Option<std::path::PathBuf>,
    /// Tool calls the harness's permission gate refused during the run.
    /// Evidence beside the outcome, never part of it: a run with denials that
    /// answered is still an answer.
    pub denials: Denials,

    pub harness: Option<HarnessForensics>,
}

/// Refused tool calls, bounded: the total count, and a short description of
/// the first few (`Denials::SHOWN`, each at most `Denials::DESCRIPTION_MAX`
/// characters), so a run that hammers a closed gate cannot grow the digest without
/// bound.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Denials {
    pub count: u32,
    pub shown: Vec<String>,
}

impl Denials {
    pub const SHOWN: usize = 5;
    pub const DESCRIPTION_MAX: usize = 200;

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Count one refused call, keeping its description while there is room.
    pub fn push(&mut self, description: &str) {
        self.count += 1;
        if self.shown.len() < Self::SHOWN {
            self.shown
                .push(super::truncate(description.trim(), Self::DESCRIPTION_MAX));
        }
    }

    /// The digest line, `None` when nothing was refused.
    pub fn line(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let noun = if self.count == 1 { "call" } else { "calls" };
        let mut line = format!(
            "permission denied: {} tool {noun}: {}",
            self.count,
            self.shown.join("; ")
        );
        let hidden = (self.count as usize).saturating_sub(self.shown.len());
        if hidden > 0 {
            line.push_str(&format!(" (+{hidden} more)"));
        }
        Some(line)
    }
}

/// Harness evidence is optional; harnesses without extra artifacts have no block.
/// This enum is internal. The documented flat DigestSummary remains the wire
/// projection so historical sidecar and audit rows never require a tagged block.
pub enum HarnessForensics {
    Codex(CodexForensics),
}

impl Digest {
    pub fn codex(&self) -> Option<&CodexForensics> {
        match self.harness.as_ref()? {
            HarnessForensics::Codex(evidence) => Some(evidence),
        }
    }
}

/// Flat, serializable projection of a `Digest` for the sidecar and audit logs.
/// Flattened (no nested transcript) so failures are greppable after the fact -
/// e.g. `jq 'select(.digest.captured==false)'`. Without this, a codex run that
/// exits non-zero mid-shutdown is filed as a clean short `response` and the exit
/// status is lost. Absent for harnesses without a machine-readable stream.
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
    /// Why `hrafnar` killed codex, when it did. Persisted so the hang class is
    /// greppable after the fact:
    /// `jq 'select(.digest.terminated_by_hrafnar != null)'`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "terminated_by_review"
    )]
    pub terminated_by_hrafnar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rollout_event: Option<String>,
    /// Codex's own stated reason for the turn ending (stream `error` /
    /// `turn.failed`). Persisted so refusals are greppable and never confused
    /// with the hang class: `jq 'select(.digest.turn_error != null)'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_error: Option<String>,
    /// hrafnar could not verify the run, even if it captured an answer.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "review_error"
    )]
    pub hrafnar_error: Option<String>,
    /// Ended by `hrafnar interrupt`: `jq 'select(.digest.interrupted)'`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    /// Tool calls the permission gate refused (claude and maki):
    /// `jq 'select(.digest.permission_denials > 0)'`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub permission_denials: u32,
    /// The first few refused calls, described (`Denials`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Digest {
    /// The flat persisted form: these forensics plus the outcome's facts,
    /// under the field names the logs have always used.
    pub fn summary(&self, outcome: &super::Outcome) -> DigestSummary {
        use super::outcome::{Answer, EndedBy};
        DigestSummary {
            exit_code: self.exit.code(),
            signal: self.exit.signal_name(),
            captured: outcome.answer == Answer::Captured,
            recovered_from_transcript: outcome.answer == Answer::Recovered,
            turns: self.turns,
            task_complete: self
                .codex()
                .and_then(|c| c.transcript.as_ref())
                .map(|t| t.task_complete),
            stream_error: self
                .codex()
                .and_then(|c| c.transcript.as_ref())
                .map(|t| t.stream_error),
            input_tokens: self.usage.total_input,
            cached_input_tokens: self.usage.cached_input,
            output_tokens: self.usage.output,
            reasoning_output_tokens: self.usage.reasoning_output,
            incident_path: self
                .incident_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            terminated_by_hrafnar: outcome.termination().map(ToString::to_string),
            quiet_secs: self.codex().and_then(|c| c.quiet_secs),
            last_rollout_event: self
                .codex()
                .and_then(|c| c.last_rollout_event.as_ref())
                .map(ToString::to_string),
            turn_error: outcome.stated.clone(),
            hrafnar_error: outcome.unverified.clone(),
            interrupted: matches!(outcome.ended_by, EndedBy::Interrupted(_)),
            permission_denials: self.denials.count,
            denied: self.denials.shown.clone(),
        }
    }

    /// The facts the outcome lines are read against.
    pub fn status_facts(&self) -> super::outcome::StatusFacts<'_> {
        super::outcome::StatusFacts {
            exit_code: self.exit.code(),
            signaled: self.exit.is_signal(),
            quiet_secs: self.codex().and_then(|c| c.quiet_secs),
            last_event: self
                .codex()
                .and_then(|c| c.last_rollout_event.as_ref())
                .map(|event| event as &dyn std::fmt::Display),
            task_complete: self
                .codex()
                .and_then(|c| c.transcript.as_ref())
                .map(|t| t.task_complete),
        }
    }
}

impl DigestSummary {
    pub fn denials(&self) -> Denials {
        Denials {
            count: self.permission_denials,
            shown: self.denied.clone(),
        }
    }

    pub fn status_facts(&self) -> super::outcome::StatusFacts<'_> {
        super::outcome::StatusFacts {
            exit_code: self.exit_code,
            // A row's recorded signal text, whatever it says, is observed
            // status: rows are evidence, never re-decoded into a live Exit.
            signaled: self.signal.is_some(),
            quiet_secs: self.quiet_secs,
            last_event: self
                .last_rollout_event
                .as_ref()
                .map(|event| event as &dyn std::fmt::Display),
            task_complete: self.task_complete,
        }
    }
}

pub struct HarnessResult {
    /// `claude_token::CREDENTIAL_TOKEN_COMMAND` when this run authenticated
    /// with a token from the configured command. The fact only.
    pub credential: Option<&'static str>,
    pub run_id: Option<crate::sessions::handoff::RunId>,
    /// Holds namespace ownership through recovery, durable recording and IPC.
    pub(crate) _reservation: Option<std::sync::Arc<crate::inflight::Reservation>>,
    pub harness: super::Harness,
    /// The turn the run established, or why none was: a spawn failure, an
    /// unknown model, a harness that ended before it showed the session
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
    /// The harness-specific permission decision for this invocation. Failed
    /// setup retains the request; successful setup carries its prepared selector. The sidecar normalizes known levels and saves
    /// this native selector separately for exact resume. Recorded because the
    /// guarantee a profile advertises is worth nothing if the permissions a run *received* are only
    /// visible in the launching terminal: `jq 'select(.sandbox=="read-only")'`
    /// has to be able to answer "which runs could not write?" after the fact.
    pub permissions: Option<super::Permissions>,
    /// Effective writable roots for this run beyond the harness's defaults
    /// (`src/writable_roots/`). Empty for every run that widened nothing,
    /// which is every read-only run.
    pub writable_roots: Vec<String>,
    /// The model and reasoning effort this run launched with, recorded for the
    /// same reason `permissions` is: they are properties of the *run*, and a
    /// `hrafnar resume` that cannot see them silently falls back to whatever
    /// the harness defaults to. `None` means "we passed nothing and the
    /// harness chose", which since `--ignore-user-config` means codex's
    /// built-in default rather than the operator's configured one.
    ///
    /// ModelSpec supplies the same selection to argv and these projections,
    /// including Codex raw selector precedence. These are launch selections,
    /// never a claim about which model the harness actually served.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// What the harness reports actually served the run. Record-only.
    pub served: Served,
    /// The folder grok was asked to trust permanently (`--trust`) for this run,
    /// when it had no trust decision yet. Recorded because the grant outlives
    /// the run and widens every later grok session there (the repo's `.grok`
    /// MCP/LSP config runs without a prompt) - a change to the operator's
    /// config that must be findable after the terminal is gone.
    pub grok_trust: Option<String>,
    /// What the run's processes left behind (see `contained`). Recorded in
    /// the audit and sidecar rows; never printed from here, since the closing
    /// report reads its command's Lifecycle ledger instead.
    pub leftovers: crate::contained::Leftovers,
}

/// What a harness reports, after the fact, about what served a run - as
/// opposed to `HarnessResult::model`, which is what we asked for.
///
/// The two are different names, not two spellings of one: grok's `-m grok-4.7`
/// is served as `grok-4.7-build`, and `-m grok-4.7-build` is refused as an
/// unknown model id. So this is recorded but **never** inherited by a
/// `hrafnar resume` - feeding it back to `-m` would fail the launch. Grok and
/// claude report it, from their result objects; empty for codex. It and
/// `cost_usd` are record-only: the row stores them and nothing reads them back.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Served {
    /// Every model that served a call in the run (one in practice; a subagent
    /// on another model would add a second). A set, so order and duplicates
    /// are settled by construction; joined only when recorded (`model_label`).
    models: std::collections::BTreeSet<String>,
    pub cost_usd: Option<f64>,
}

impl Served {
    pub fn new(models: impl IntoIterator<Item = String>, cost_usd: Option<f64>) -> Self {
        Self {
            models: models.into_iter().collect(),
            cost_usd,
        }
    }

    /// The same facts with `secret` replaced wherever it appears in a model
    /// name, for a run whose child held a credential hrafnar supplied.
    pub(super) fn scrubbed(self, secret: &str, with: &str) -> Self {
        Self {
            models: self
                .models
                .into_iter()
                .map(|m| m.replace(secret, with))
                .collect(),
            cost_usd: self.cost_usd,
        }
    }

    /// The recorded form: sorted and comma-joined, `None` when none reported.
    pub fn model_label(&self) -> Option<String> {
        (!self.models.is_empty()).then(|| {
            self.models
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
    }

    /// Fold in a later invocation of the same run: models are a union; costs
    /// sum where known, and an unknown cost does not erase a known one.
    pub fn merge(&mut self, other: Served) {
        self.models.extend(other.models);
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
    }
}

pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Internal result of one harness run before it's wrapped into a
/// `HarnessResult`. Lets the run helpers return the session ID and digest
/// alongside the text without a widening tuple.
pub(super) struct RunOutput {
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

/// Record-only served fields from Grok's single result object. Claude-compatible
/// streams sum every result through claude_stream::ClaudeMetrics instead: the
/// coincident field names do not make these protocols interchangeable.
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
    Served::new(raw.model_usage.keys().cloned(), raw.total_cost_usd)
}

/// The process status of a grok or claude run, when it is worth keeping: a
/// non-zero or unknown exit after an answer is still an answer (the outcome
/// says so), but the status is the only sign something went wrong after the
/// turn, so it is recorded. `None` for a clean exit.
pub(super) fn answered_status_digest(status: crate::process::Exit) -> Option<Digest> {
    (!status.success()).then(|| status_digest(status))
}

/// A grok or claude run's observed process status as forensics. The *observed*
/// status, never an assumed one: the exit code is what an operator reads to
/// tell "the harness declined" from "the harness was killed", and `Unknown`
/// means the keeper could not report it.
pub(super) fn status_digest(status: crate::process::Exit) -> Digest {
    Digest {
        exit: status,
        ..Digest::default()
    }
}

#[cfg(test)]
#[path = "result_tests.rs"]
mod tests;
