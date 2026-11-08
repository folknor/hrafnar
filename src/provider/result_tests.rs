//! Unit tests of the result digests shared by every runner.

use super::super::tests::exited;

#[test]
fn post_answer_status_is_recorded_without_inventing_a_turn_failure() {
    assert!(super::answered_status_digest(Some(&exited(0))).is_none());
    for status in [None, Some(exited(7))] {
        let digest = super::answered_status_digest(status.as_ref()).expect("status digest");
        assert_eq!(
            digest.exit_code,
            status.as_ref().and_then(std::process::ExitStatus::code)
        );
        // The outcome, not the status, decides: an answer with a bad exit is
        // still an answer and does not fail the run.
        let outcome = super::super::Outcome::answered(super::super::Answer::Captured);
        let summary = digest.summary(&outcome);
        assert!(summary.captured);
        assert!(summary.turn_error.is_none() && summary.review_error.is_none());
        assert!(!outcome.failed());
    }
}
