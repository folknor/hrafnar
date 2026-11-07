//! Unit tests of the result digests shared by every runner.

use super::super::provider_tests::exited;

#[test]
fn post_answer_status_is_shared_without_inventing_a_turn_failure() {
    assert!(super::answered_status_digest(Some(&exited(0))).is_none());
    for status in [None, Some(exited(7))] {
        let digest = super::answered_status_digest(status.as_ref()).expect("status digest");
        assert!(digest.captured);
        assert!(!digest.failed());
        assert!(digest.turn_error.is_none());
        assert!(digest.review_error.is_none());
        assert_eq!(
            digest.exit_code,
            status.as_ref().and_then(std::process::ExitStatus::code)
        );
    }
}
