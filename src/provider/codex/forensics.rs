//! Codex-only evidence, rendering and bundle artifacts. Generic reporting and
//! bundle storage dispatch here, without knowing how to read a Codex rollout.
use crate::incident::{IncidentFailure, tail_lines, write_file};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct CodexForensics {
    /// Non-JSON stdout lines (codex ERROR/WARN, apply_patch dumps). The harness
    /// can halt NDJSON emission on these, so we keep them visible.
    pub log_lines: Vec<String>,
    /// On-disk transcript forensics, read only when the run looks wrong
    /// (not captured, non-zero exit, or killed by a signal).
    pub transcript: Option<crate::transcript::TranscriptSummary>,
    /// How long the rollout had been unchanged when the watchdog decided to act.
    /// Recorded so a stall-timeout misfire on a healthy run shows how close the
    /// call was, rather than only that it happened.
    pub quiet_secs: Option<u64>,
    /// `type/payload.type` of the last rollout event - what codex was doing when
    /// it went silent.
    pub last_rollout_event: Option<String>,
}

pub struct CodexIncident<'a> {
    pub transcript_path: Option<&'a str>,
    /// The same scope passed to transcript recovery. An offset beyond EOF
    /// means no bytes can safely be attributed to this run.
    pub transcript_offset: u64,
    /// Whether recovery (including a preserved watchdog answer) found an answer.
    pub final_answer_present: Option<bool>,
    /// How long the rollout had been unchanged when the watchdog acted, and the
    /// last event it saw. The pair that makes a stall-timeout misfire on a
    /// healthy run diagnosable after the fact.
    pub quiet_secs: Option<u64>,
    pub last_rollout_event: Option<&'a str>,
}

impl CodexForensics {
    pub(crate) fn print(&self) {
        let c = self;
        if !c.log_lines.is_empty() {
            println!("--- codex log lines ({}) ---", c.log_lines.len());
            for line in &c.log_lines {
                println!("{line}");
            }
            println!("--- end log lines ---");
        }
        if let Some(ref t) = c.transcript {
            print_transcript(t);
        }
    }
}

pub(crate) fn print_transcript(t: &crate::transcript::TranscriptSummary) {
    println!("transcript: {}", t.path);
    println!(
        "  task_complete={} stream_error={}",
        t.task_complete, t.stream_error
    );
    if let Some(ref last) = t.last_event {
        println!("  last_event: {last}");
    }
    if let Some(ref tool) = t.last_in_flight_tool {
        println!("  last_in_flight_tool: {tool}");
    }
}

impl CodexIncident<'_> {
    pub(crate) fn write_artifacts(
        &self,
        dir: &Path,
    ) -> (Option<serde_json::Value>, Vec<IncidentFailure>) {
        let mut failures = Vec::new();
        // This excerpt and metadata are scoped to exactly the recovery baseline.
        let mut rate_limits = None;
        if let Some(path) = self.transcript_path {
            match std::fs::read(path) {
                Ok(bytes) => {
                    let scoped =
                        crate::transcript::slice_from_offset(&bytes, Some(self.transcript_offset));
                    let content = String::from_utf8_lossy(scoped);
                    let tail = tail_lines(&content, TRANSCRIPT_TAIL_LINES);
                    if let Err(error) = write_file(dir, "transcript.tail.jsonl", tail.as_bytes()) {
                        failures.push(error);
                    }
                    rate_limits = scan_rate_limits(&content);
                }
                Err(error) => failures.push(IncidentFailure {
                    path: Some(PathBuf::from(path)),
                    message: format!("failed to read rollout transcript: {error}"),
                }),
            }
        }

        (rate_limits, failures)
    }
}

/// Raw transcript lines to keep in the tail (the full file stays on disk).
const TRANSCRIPT_TAIL_LINES: usize = 80;

/// Extract the last non-null rate-limit snapshot from an already run-scoped
/// rollout excerpt. Answer recognition belongs to the shared rollout fold.
fn scan_rate_limits(content: &str) -> Option<serde_json::Value> {
    crate::provider::codex::rollout::fold(content).rate_limits
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scan_keeps_the_last_non_null_rate_limit_snapshot() {
        let rollout = concat!(
            r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":31.0}}}}"#,
            "\n",
            r#"{"payload":{"type":"token_count","rate_limits":null}}"#,
        );
        assert_eq!(
            scan_rate_limits(rollout),
            Some(serde_json::json!({"primary":{"used_percent":31.0}}))
        );
    }

    #[test]
    fn scan_skips_malformed_lines() {
        let rollout = concat!(
            "not json\n",
            r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":31.0}}}}"#,
            "\n",
        );
        assert_eq!(
            scan_rate_limits(rollout),
            Some(serde_json::json!({"primary":{"used_percent":31.0}}))
        );
    }
}
