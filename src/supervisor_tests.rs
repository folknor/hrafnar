use super::*;
use crate::provider::{Provider, ProviderRuntime};

fn control() -> Arc<Control> {
    Arc::new(Control(
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("null")
            .into(),
    ))
}

fn unknown() -> Leftovers {
    Leftovers {
        unknown: Some("unconfirmed teardown".to_string()),
        ..Leftovers::default()
    }
}

#[test]
fn runtime_clones_share_a_domain_but_new_runtimes_are_isolated() {
    let first = ProviderRuntime::default();
    let cloned = first.clone();
    let other = ProviderRuntime::default();
    let ctl = control();
    first
        .lifecycle
        .0
        .registry
        .lock()
        .expect("registry")
        .live
        .insert(7, Arc::clone(&ctl));
    first.lifecycle.complete("abandoned", 7, &ctl, &unknown());
    first.lifecycle.request_stop();
    assert!(cloned.lifecycle.stop_requested());
    assert_eq!(cloned.lifecycle.unresolved().len(), 1);
    assert!(other.lifecycle.unresolved().is_empty());
    assert!(!other.lifecycle.stop_requested());
    assert!(other.lifecycle.live_keepers().is_empty());
}

#[test]
fn recording_guard_survives_completion_and_stop_but_no_new_guard_is_admitted() {
    let lifecycle = Lifecycle::default();
    let cloned = lifecycle.clone();
    let guard = lifecycle.unrecorded_for(Provider::Codex).expect("guard");
    assert!(lifecycle.unrecorded_for(Provider::Grok).is_none());
    let ctl = control();
    lifecycle.complete("finished before recording", 7, &ctl, &Leftovers::default());
    assert_eq!(
        cloned.unrecorded(),
        1,
        "teardown does not release recording"
    );
    lifecycle.request_stop();
    assert!(cloned.unrecorded_for(Provider::Claude).is_none());
    assert_eq!(cloned.unrecorded(), 1);
    drop(guard);
    assert_eq!(cloned.unrecorded(), 0);
    assert!(
        *cloned.subscribe().borrow(),
        "late subscriber sees latched stop"
    );
}

#[tokio::test]
async fn stopping_refuses_spawn_in_the_same_domain_only() {
    let lifecycle = Lifecycle::default();
    let ctl = control();
    lifecycle.request_stop();
    // The nonexistent executable distinguishes admission refusal from exec
    // failure without starting any uncontained test process.
    let mut command = Command::new("/review-nonexistent-test-command");
    let error = lifecycle
        .spawn(&mut command, &ctl, std::time::Duration::from_secs(7))
        .expect_err("refused");
    assert!(error.to_string().contains("not launched"), "{error:#}");
    assert!(lifecycle.live_keepers().is_empty());
    let other = Lifecycle::default();
    let error = other
        .spawn(&mut command, &ctl, std::time::Duration::from_secs(7))
        .expect_err("exec fails");
    assert!(error.to_string().contains("failed to spawn"), "{error:#}");
}

#[test]
fn completion_cannot_remove_another_registration_that_reused_the_pid() {
    let lifecycle = Lifecycle::default();
    let old = control();
    let new = control();
    lifecycle
        .0
        .registry
        .lock()
        .expect("registry")
        .live
        .insert(7, Arc::clone(&new));
    lifecycle
        .0
        .registry
        .lock()
        .expect("registry")
        .bounds
        .insert(7, std::time::Duration::from_secs(17));
    lifecycle.complete("old run", 7, &old, &unknown());
    assert_eq!(
        lifecycle
            .0
            .registry
            .lock()
            .expect("registry")
            .bounds
            .get(&7),
        Some(&std::time::Duration::from_secs(17))
    );
    assert_eq!(lifecycle.live_keepers(), vec![7]);
    assert_eq!(lifecycle.unresolved().len(), 1);
    lifecycle.complete("new run", 7, &new, &Leftovers::default());
    assert!(lifecycle.live_keepers().is_empty());
    assert!(
        lifecycle
            .0
            .registry
            .lock()
            .expect("registry")
            .bounds
            .is_empty()
    );
    assert_eq!(
        lifecycle.unresolved().len(),
        1,
        "later clean run cannot erase evidence"
    );
}

#[tokio::test]
async fn poisoned_registry_refuses_admission_and_preserves_accounting() {
    let lifecycle = Lifecycle::default();
    let ctl = control();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut registry = lifecycle.0.registry.lock().expect("registry");
        registry.live.insert(7, Arc::clone(&ctl));
        panic!("poison test");
    }));
    let error = lifecycle
        .spawn(
            &mut Command::new("/review-nonexistent-test-command"),
            &ctl,
            std::time::Duration::from_secs(7),
        )
        .expect_err("poison refuses admission");
    assert!(error.to_string().contains("registry unavailable"));
    lifecycle.request_stop();
    assert!(lifecycle.stop_requested());
    assert_eq!(lifecycle.live_keepers(), vec![7]);
    lifecycle.complete("poisoned owner", 7, &ctl, &unknown());
    assert!(lifecycle.live_keepers().is_empty());
    assert_eq!(lifecycle.unresolved().len(), 1);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ledger = lifecycle.0.ledger.lock().expect("ledger");
        panic!("poison ledger");
    }));
    assert_eq!(
        lifecycle.unresolved().len(),
        1,
        "never substitute an empty ledger"
    );
}

#[test]
fn concurrent_guard_registration_and_stop_leave_no_late_obligations() {
    for _ in 0..32 {
        let lifecycle = Lifecycle::default();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let other = lifecycle.clone();
            let start = Arc::clone(&barrier);
            let register = scope.spawn(move || {
                start.wait();
                other.unrecorded_for(Provider::Claude)
            });
            barrier.wait();
            lifecycle.request_stop();
            let count_after_stop = lifecycle.unrecorded();
            let guard = register.join().expect("register thread");
            assert_eq!(count_after_stop, usize::from(guard.is_some()));
            assert!(lifecycle.unrecorded_for(Provider::Claude).is_none());
            drop(guard);
            assert_eq!(lifecycle.unrecorded(), 0);
        });
    }
}

#[test]
fn graceful_stop_notifies_without_sending_forced_termination() {
    use std::io::Read;
    let lifecycle = Lifecycle::default();
    let (write, mut read) = std::os::unix::net::UnixStream::pair().expect("pair");
    read.set_nonblocking(true).expect("nonblocking");
    let ctl = Arc::new(Control(write.into()));
    lifecycle
        .0
        .registry
        .lock()
        .expect("registry")
        .live
        .insert(7, Arc::clone(&ctl));
    lifecycle.request_stop();
    let mut byte = [0];
    assert_eq!(
        read.read(&mut byte)
            .expect_err("no forced termination")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(lifecycle.terminate_all(), 1);
    assert_eq!(read.read(&mut byte).expect("termination byte"), 1);
    assert_eq!(byte[0], keeper::Control::Terminate.byte());
    lifecycle.complete("cleanly ended", 7, &ctl, &Leftovers::default());
    assert!(lifecycle.live_keepers().is_empty());
}
