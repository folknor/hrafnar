//! `review interrupt <ID>`: stop a codex, claude or maki run mid-turn and hand back
//! its session.
//!
//! # Why a verb
//!
//! A `codex exec` turn cannot be spoken to while it runs - it reads stdin once,
//! listens on nothing, and its session is locked against every other writer
//! (see `reference/codex.md`). The only way to redirect a long run is to end
//! the turn and resume the session with a new message. Doing that by hand meant
//! finding codex's pid, and then losing the race with our own auto-resume,
//! which reads an interrupted turn as a mid-turn death and resumes straight past
//! it. This verb does the whole thing and ends by printing the resume command.
//! A `claude --print` turn is no more reachable, and ends the same way on
//! `SIGINT` (see `reference/claude.md`), so it takes the same path.
//!
//! # How
//!
//! 1. The run's in-flight marker names its keeper's pid (see `keeper`), the
//!    owning `review`'s child and leader of its own process group - offered
//!    only once the provider has produced output, so the provider is never too
//!    young to handle the signal (for codex, a node wrapper that would die on
//!    it).
//! 2. An interrupt request is written beside the marker *before* the signal, so
//!    the owning `review` can tell this apart from a death once codex exits.
//! 3. `SIGINT` goes to the keeper, which forwards one SIGINT to the provider
//!    alone, through a pidfd - on a codex npm install the node wrapper, which
//!    forwards it to the native binary once. Signalling the whole group would
//!    hand the native binary a second copy. codex answers `SIGINT` with a
//!    `turn/interrupt`, so the turn ends cleanly and the session lock is freed;
//!    claude ends the turn, kills its running command and prints its result.
//! 4. We wait for the owner to consume the request (the provider has been reaped),
//!    then for its exact run handoff, published after recovery and recording.
//!    Printing the resume command any earlier would hand the operator a command
//!    that fails.

use anyhow::{Result, bail};
use std::path::Path;

use crate::sessions::{handoff, query};

pub async fn run(session_id: &str) -> Result<()> {
    let outcome = interrupt(session_id, None).await?;
    println!("session: {session_id}");
    println!("project: {}", outcome.project);
    if outcome.interrupted {
        println!(
            "{}",
            crate::provider::resume_hint(outcome.provider, session_id)
        );
    } else {
        // The run finished before the signal took effect; its answer is in the
        // owner's output.
        println!(
            "the run ended before the interrupt took effect - see its output, or \
             `review sessions {session_id}`"
        );
    }
    Ok(())
}

/// What became of the captured invocation, after its durable handoff.
#[derive(Debug)]
pub struct Outcome {
    pub interrupted: bool,
    pub project: String,
    pub provider: crate::provider::Provider,
}

/// Target one observed invocation, never a row position or a replacement run.
pub async fn interrupt(session_id: &str, data_root: Option<&Path>) -> Result<Outcome> {
    let observation = query::Observation::capture(data_root);
    let empty = query::Snapshot::empty();
    let marker = match observation.state(session_id, &empty) {
        query::State::InFlight(m) => m,
        query::State::HandoffPending(m) => m,
        query::State::CleanupPending(m) => bail!("{}", cleanup_pending_message(session_id, m)),
        query::State::Ambiguous => {
            bail!("session {session_id}: ambiguous live-run observation; retry shortly")
        }
        query::State::Queued(_) => bail!(
            "a message to session {session_id} is waiting to launch, so there is no turn to interrupt yet\n  retry once it has launched, or send your own `review message`, which waits for it and then interrupts it"
        ),
        query::State::Idle(_) | query::State::Unknown => bail!(
            "no run of session {session_id} is in flight on this host\n  `review sessions --all` lists the ones that are"
        ),
    };
    interrupt_observed(session_id, marker, data_root).await
}

/// Message has already selected and project-validated this exact observation.
/// Reacquiring a different target here could interrupt an unvalidated project.
pub(crate) async fn interrupt_observed(
    session_id: &str,
    marker: &crate::inflight::Live,
    data_root: Option<&Path>,
) -> Result<Outcome> {
    let data = crate::data::DataDir::from_override(data_root);
    let empty = query::Snapshot::empty();
    let pending = marker.phase == crate::inflight::Phase::HandoffPending;
    if marker.cleanup_pending() {
        bail!("{}", cleanup_pending_message(session_id, marker));
    }
    let provider = match marker.provider.parse::<crate::provider::Provider>() {
        Ok(p) if pending || p.interruptible() => p,
        _ => bail!(
            "session {session_id} is a {} run; only {} runs can be interrupted",
            marker.provider,
            crate::provider::Provider::interruptible_names()
        ),
    };
    // Older markers have run_id too; only the explicit protocol promises a
    // completion receipt. Refuse before changing a turn we cannot hand back.
    anyhow::ensure!(
        marker.protocol == handoff::PROTOCOL,
        "session {session_id}: this run was launched by an older review without result handoff support; wait for it to finish"
    );
    let run_id: handoff::RunId = marker.run_id.clone().try_into()?;
    if pending {
        return wait_handoff(session_id, marker, provider, &data, &run_id).await;
    }
    let owner = marker.owner;
    let mut current = marker.clone();
    let deadline = std::time::Instant::now() + crate::timings::INTERRUPT_PID_WAIT;
    while !current.signal_ready {
        if let Some(done) = handoff::read(&data, &run_id)? {
            return handoff_outcome(session_id, marker, provider, &done);
        }
        let observed = query::Observation::capture(data_root);
        match observed.state(session_id, &empty) {
            query::State::Ambiguous => {
                bail!("session {session_id}: ambiguous live-run observation; retry shortly")
            }
            query::State::CleanupPending(m) => bail!("{}", cleanup_pending_message(session_id, m)),
            query::State::InFlight(m) if m.run_id == marker.run_id => current = m.clone(),
            _ => return wait_handoff(session_id, marker, provider, &data, &run_id).await,
        }
        if current.signal_ready {
            break;
        }
        if owner.is_gone() {
            return wait_handoff(session_id, marker, provider, &data, &run_id).await;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "session {session_id} cannot be interrupted: {provider} has produced no output in {}s (review pid {})",
            crate::timings::INTERRUPT_PID_WAIT.as_secs(),
            owner.pid()
        );
        tokio::time::sleep(crate::timings::INTERRUPT_PID_POLL).await;
    }
    // Reclassify immediately before signalling, even if the first snapshot was
    // already ready. A newer recovery may supersede the captured target, but
    // that never redirects an existing interrupter to a different invocation.
    let observed = query::Observation::capture(data_root);
    let current = match observed.state(session_id, &empty) {
        query::State::Ambiguous => {
            bail!("session {session_id}: ambiguous live-run observation; retry shortly")
        }
        query::State::CleanupPending(m) => bail!("{}", cleanup_pending_message(session_id, m)),
        query::State::InFlight(m)
            if m.run_id == marker.run_id && m.owner == owner && m.signal_ready =>
        {
            m
        }
        _ => return wait_handoff(session_id, marker, provider, &data, &run_id).await,
    };
    let keeper = current
        .keeper
        .ok_or_else(|| anyhow::anyhow!("session {session_id}: keeper is not recorded"))?;
    let pinned = match keeper.pin() {
        Ok(p) if p.is_keeper_of(owner) => p,
        _ => return wait_handoff(session_id, marker, provider, &data, &run_id).await,
    };
    // Exact request paths make even a late request harmless to later runs.
    marker.request(&data)?;
    if let Err(e) = pinned.signal(libc::SIGINT) {
        eprintln!("keeper signal failed ({e}); waiting for this run's handoff");
    } else {
        eprintln!(
            "interrupt sent to {provider} through its keeper (pid {}); waiting for the run to wind down",
            keeper.pid()
        );
    }
    wait_handoff(session_id, marker, provider, &data, &run_id).await
}
fn handoff_outcome(
    session_id: &str,
    marker: &crate::inflight::Live,
    provider: crate::provider::Provider,
    done: &handoff::Handoff,
) -> Result<Outcome> {
    anyhow::ensure!(
        done.sidecar_written && done.final_session.as_deref() == Some(session_id),
        "session {session_id}: the run finished without a recorded handoff for this session; inspect its owner's output"
    );
    Ok(Outcome {
        interrupted: done.interrupted,
        project: marker.project.clone(),
        provider,
    })
}
async fn wait_handoff(
    session_id: &str,
    marker: &crate::inflight::Live,
    provider: crate::provider::Provider,
    data: &crate::data::DataDir,
    run_id: &handoff::RunId,
) -> Result<Outcome> {
    loop {
        if let Some(done) = handoff::read(data, run_id)? {
            return handoff_outcome(session_id, marker, provider, &done);
        }
        if marker.owner.is_gone() {
            // Publication may race the liveness observation. A final read is
            // mandatory before declaring the handoff incomplete.
            if let Some(done) = handoff::read(data, run_id)? {
                return handoff_outcome(session_id, marker, provider, &done);
            }
            bail!(
                "the owning review (pid {}) exited without recording a handoff for session {session_id}",
                marker.owner.pid()
            );
        }
        tokio::time::sleep(crate::timings::INTERRUPT_POLL).await;
    }
}

/// Why nothing may launch on, or interrupt, a session whose owning `review` is
/// gone while its keeper still tears the run down. Shared with `review
/// message`.
// This wording is CLI policy: the store reports liveness, while message
// imports the interrupt verb's explanation rather than teaching storage prose.
pub fn cleanup_pending_message(session_id: &str, marker: &crate::inflight::Live) -> String {
    format!(
        "session {session_id}: the review that ran it (pid {}) is gone and its processes are \
         still being torn down (keeper pid {}); retry shortly",
        marker.owner.pid(),
        marker
            .keeper
            .map_or_else(|| "?".to_string(), |k| k.pid().to_string())
    )
}

/// Whether a recorded run of `provider` (a marker's string) can be ended
/// mid-turn and resumed: `Provider::interruptible`, which the signal
/// supervisor's `Unrecorded` reads too. A name `review` does not drive is not.
pub(crate) fn interruptible(provider: &str) -> bool {
    provider
        .parse::<crate::provider::Provider>()
        .is_ok_and(crate::provider::Provider::interruptible)
}

#[cfg(test)]
mod handoff_tests {
    use super::*;
    #[tokio::test]
    async fn pending_handoff_waits_without_signalling_or_guessing_from_old_rows() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        let data = crate::data::DataDir::at(root.clone());
        let id = handoff::RunId::default();
        let reserved = crate::inflight::reserve(&data, &id)
            .expect("reserve")
            .expect("storage");
        let sid = crate::provider::SessionId::parse("pending-session").expect("session");
        let mut guard = crate::inflight::mark_for(
            &sid,
            crate::provider::Provider::Grok,
            "/p",
            &data,
            &id,
            None,
        );
        reserved.register(&guard);
        guard.handoff();
        drop(guard);
        let marker = crate::inflight::live_marker(sid.as_str(), Some(&root))
            .expect("continuous pending marker");
        assert_eq!(marker.phase, crate::inflight::Phase::HandoffPending);
        let scope = crate::data::LogScope::new(
            crate::data::AuditId::from_record("a").expect("audit"),
            false,
        );
        let mut row: crate::sessions::SessionRow = serde_json::from_value(serde_json::json!({
            "timestamp": "old", "epoch_secs": 1, "project": "/p", "hostname": "h", "audit_id": "a",
            "provider": "grok", "archetype": "bare", "session_id": sid.as_str(),
            "operator_prompt": "old", "assembled_prompt": "old", "review_version": "0",
        }))
        .expect("row");
        assert!(crate::sessions::append(&data, &scope, &row));
        let waiter = interrupt(sid.as_str(), Some(&root));
        let recorder = async {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            assert!(handoff::read(&data, &id).expect("receipt").is_none());
            row.epoch_secs = Some(2);
            assert!(crate::sessions::append(&data, &scope, &row));
            handoff::publish(
                &data,
                &handoff::Handoff {
                    schema: handoff::PROTOCOL,
                    requested: id.clone(),
                    interrupted: false,
                    final_session: Some(sid.as_str().to_owned()),
                    sidecar_written: true,
                },
            )
            .expect("publish");
            reserved.complete();
        };
        let (outcome, ()) = tokio::join!(waiter, recorder);
        assert!(!outcome.expect("recorded handoff").interrupted);
        assert_eq!(crate::sessions::read_all_in(&data).len(), 2);
        assert!(crate::inflight::live_marker(sid.as_str(), Some(&root)).is_none());
        drop(reserved);
        let _ = std::fs::remove_dir_all(root);
    }
}
