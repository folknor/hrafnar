//! Unit tests of the result digests shared by every runner.

use crate::process::Exit;

#[test]
fn post_answer_status_is_recorded_without_inventing_a_turn_failure() {
    assert!(super::answered_status_digest(Exit::Code(0)).is_none());
    for status in [Exit::Unknown, Exit::Code(7)] {
        let digest = super::answered_status_digest(status).expect("status digest");
        assert_eq!(digest.exit, status);
        // The outcome, not the status, decides: an answer with a bad exit is
        // still an answer and does not fail the run.
        let outcome = super::super::Outcome::answered(super::super::Answer::Captured);
        let summary = digest.summary(&outcome);
        assert!(summary.captured);
        assert!(summary.turn_error.is_none() && summary.hrafnar_error.is_none());
        assert!(!outcome.failed());
    }
}

#[test]
fn harness_extension_projects_the_unchanged_flat_digest_schema() {
    use super::super::{Answer, CodexForensics, Digest, HarnessForensics, Outcome};
    let legacy = serde_json::json!({
        "exit_code": null, "signal": "SIGKILL", "captured": false,
        "recovered_from_transcript": true, "turns": 2,
        "task_complete": true, "stream_error": false,
        "input_tokens": 0, "cached_input_tokens": 0,
        "output_tokens": 0, "reasoning_output_tokens": 0,
        "incident_path": "/bundle", "terminated_by_hrafnar": "watchdog: stranded completion",
        "quiet_secs": 30, "last_rollout_event": "event_msg/task_complete"
    });
    let parsed: super::DigestSummary = serde_json::from_value(legacy.clone()).expect("old row");
    assert_eq!(serde_json::to_value(&parsed).expect("round trip"), legacy);
    let digest = Digest {
        exit: Exit::Signal(libc::SIGKILL),
        turns: 2,
        incident_path: Some("/bundle".into()),
        harness: Some(HarnessForensics::Codex(CodexForensics {
            transcript: Some(
                crate::harness::codex::rollout::fold("{\"payload\":{\"type\":\"task_complete\"}}")
                    .current_turn,
            ),
            quiet_secs: Some(30),
            last_rollout_event: Some(crate::transcript::EventKind {
                top: Some("event_msg".into()),
                payload: Some("task_complete".into()),
            }),
            ..CodexForensics::default()
        })),
        ..Digest::default()
    };
    let outcome = Outcome::answered(Answer::Recovered).ended(
        false,
        false,
        Some(crate::harness::Termination::Watchdog(
            crate::harness::WatchdogTermination::Stranded,
        )),
    );
    assert_eq!(
        serde_json::to_value(digest.summary(&outcome)).expect("projection"),
        legacy
    );
    let generic = Digest::default().summary(&Outcome::no_answer());
    assert!(generic.task_complete.is_none() && generic.quiet_secs.is_none());
}
