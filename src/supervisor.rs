//! Command-owned stop, recording, keeper admission and teardown evidence.
//! Signal registration is process-wide; the state captured by it is not global.

use crate::{contained::Leftovers, keeper};
use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::process::{Child, Command};

/// One invocation whose teardown did not establish that nothing is alive.
#[derive(Clone, Debug)]
pub struct Unresolved {
    pub label: String,
    pub keeper_pid: u32,
    pub leftovers: Leftovers,
}

/// The control pipe's write end, shared by a run and the registry.
pub(crate) struct Control(pub(crate) OwnedFd);

impl Control {
    pub(crate) fn send(&self, byte: u8) -> std::io::Result<()> {
        // SAFETY: one byte from a local into a descriptor we own; the pipe is
        // non-blocking, and the keeper drains it promptly.
        let n = unsafe {
            libc::write(
                self.0.as_raw_fd(),
                (&raw const byte).cast::<libc::c_void>(),
                1,
            )
        };
        if n == 1 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[derive(Default)]
struct Registry {
    closed: bool,
    live: std::collections::HashMap<u32, Arc<Control>>,
}

/// Shared by every attempt, wake and probe belonging to one command. Default
/// creates a fresh domain; cloning preserves it. No process-global fallback.
#[derive(Clone)]
pub struct Lifecycle(Arc<State>);

struct State {
    registry: Mutex<Registry>,
    ledger: Mutex<Vec<Unresolved>>,
    stop: tokio::sync::watch::Sender<bool>,
    unrecorded: AtomicUsize,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self(Arc::new(State {
            registry: Mutex::new(Registry::default()),
            ledger: Mutex::new(Vec::new()),
            stop: tokio::sync::watch::channel(false).0,
            unrecorded: AtomicUsize::new(0),
        }))
    }
}

impl Lifecycle {
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.0.stop.subscribe()
    }

    pub fn stop_requested(&self) -> bool {
        *self.0.stop.borrow()
    }

    /// Close admission and notify turns without terminating their keepers.
    /// The registry mutex serializes this transition with spawn and recording
    /// guard registration. Never retain a watch borrow while taking this lock.
    pub fn request_stop(&self) {
        let mut registry = self
            .0
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.closed = true;
        self.0.stop.send_replace(true);
    }

    pub(crate) fn spawn(&self, cmd: &mut Command, control: &Arc<Control>) -> Result<Child> {
        let mut registry = self
            .0
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("keeper registry unavailable"))?;
        if registry.closed {
            anyhow::bail!("not launched: review was signalled to stop");
        }
        let child = cmd.spawn().context("failed to spawn the keeper")?;
        if let Some(pid) = child.id() {
            registry.live.insert(pid, Arc::clone(control));
        }
        Ok(child)
    }

    pub(crate) fn complete(
        &self,
        label: &str,
        pid: u32,
        control: &Arc<Control>,
        leftovers: &Leftovers,
    ) {
        // Publish evidence before registry removal: an empty registry must not
        // let the supervisor outrun the verdict. Recover original poisoned data
        // for accounting rather than inventing a clean/empty replacement.
        if !leftovers.is_clean() {
            self.0
                .ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Unresolved {
                    label: label.to_string(),
                    keeper_pid: pid,
                    leftovers: leftovers.clone(),
                });
        }
        let mut registry = self
            .0
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry
            .live
            .get(&pid)
            .is_some_and(|owner| Arc::ptr_eq(owner, control))
        {
            registry.live.remove(&pid);
        }
    }

    pub fn unresolved(&self) -> Vec<Unresolved> {
        self.0
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn live_keepers(&self) -> Vec<u32> {
        self.0
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live
            .keys()
            .copied()
            .collect()
    }

    /// Escalation, separate from graceful stop. Admission stays closed.
    pub fn terminate_all(&self) -> usize {
        let mut registry = self
            .0
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.closed = true;
        for control in registry.live.values() {
            let _ = control.send(keeper::Control::Terminate.byte());
        }
        registry.live.len()
    }

    /// Only interruptible turns owe a graceful-stop recording wait. Refusal
    /// after stop is safe: this same domain also refuses the ensuing spawn.
    pub fn unrecorded_for(&self, provider: crate::provider::Provider) -> Option<Unrecorded> {
        let registry = self
            .0
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.closed || !provider.interruptible() {
            return None;
        }
        self.0.unrecorded.fetch_add(1, Ordering::SeqCst);
        Some(Unrecorded(self.clone()))
    }

    fn unrecorded(&self) -> usize {
        self.0.unrecorded.load(Ordering::SeqCst)
    }
}

/// Held through sidecar recording, beyond provider teardown. Not clonable:
/// exactly one drop releases each registered recording obligation.
pub struct Unrecorded(Lifecycle);

impl Drop for Unrecorded {
    fn drop(&mut self) {
        self.0.0.unrecorded.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Install the one process-wide handler for `SIGINT`/`SIGTERM`.
///
/// This is deliberately a single global supervisor rather than a `select!` arm
/// inside each run. Tokio's signal registration is process-wide and permanent -
/// once a `SignalKind::terminate()` stream has been created, the default
/// "terminate the process" behaviour is gone for the rest of the run, even after
/// the stream is dropped. Installing it per-run would therefore have left
/// `review` quietly ignoring every `SIGTERM` after its first codex invocation,
/// i.e. *harder* to kill than before - the opposite of the intent.
///
/// With codex or claude runs in flight the first signal is a graceful stop: each run ends
/// its turn as `review interrupt` would, records its session, and only then does
/// `review` exit. The signal is most often an orchestrating agent stopping the
/// background task it launched `review` as - asked to "interrupt the agent",
/// Claude reaches for its own task-stop tool, not for `review interrupt` - and
/// the old kill-and-exit left a fresh session with no sidecar row, so it could
/// never be resumed. A second signal, or `GRACEFUL_STOP` passing, falls through
/// to the kill below, which forfeits the digest and the row; the rollout
/// transcript is on disk regardless, for `review sessions <id>`.
///
/// Either way `review` then terminates every keeper still running (grok runs,
/// and whatever the graceful stop did not end), waits for their teardowns up to
/// `contained::Settings::terminate_bound`, and reports anything left running or
/// unaccounted for before it exits. A keeper still tearing down at that bound
/// is named as such; it finishes on its own, writing to the orphan log.
///
/// `SIGKILL` cannot be caught, so a stop that sends only that still loses the
/// row. The keepers see their control pipes close and tear down regardless.
pub fn install_signal_supervisor(lifecycle: Lifecycle) {
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::spawn(async move {
        let mut next_signal = async || match sigterm.as_mut() {
            Some(term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                }
            }
            // SIGTERM handler unavailable: still forward Ctrl-C, and leave
            // SIGTERM on its OS default (which kills us, orphaning codex - the
            // old behaviour, and better than not handling anything).
            None => {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        };
        let name = next_signal().await;
        let exit_code = if name == "SIGINT" { 130 } else { 143 };
        lifecycle.request_stop();
        let unrecorded = || lifecycle.unrecorded();
        if unrecorded() > 0 {
            eprintln!(
                "\n{name}: ending {} turn(s) so their sessions stay resumable \
                 (signal again to kill instead)",
                unrecorded()
            );
            let graceful = async {
                while unrecorded() > 0 {
                    tokio::time::sleep(crate::timings::GRACEFUL_STOP_POLL).await;
                }
            };
            tokio::select! {
                () = graceful => {}
                () = tokio::time::sleep(crate::timings::GRACEFUL_STOP) => {
                    eprintln!("a turn did not end in time; tearing down what is left");
                }
                _ = next_signal() => {}
            }
        }
        // No keeper spawns from here, and every live one tears down its run.
        let told = lifecycle.terminate_all();
        if told > 0 {
            eprintln!("\n{name}: tearing down {told} running provider(s)");
        }
        let bound = crate::contained::Settings::default().terminate_bound();
        let drained = async {
            while !lifecycle.live_keepers().is_empty() {
                tokio::time::sleep(crate::timings::GRACEFUL_STOP_POLL).await;
            }
        };
        tokio::select! {
            () = drained => {}
            () = tokio::time::sleep(bound) => {}
            _ = next_signal() => {}
        }
        for line in crate::report::report_lines(&lifecycle.unresolved()) {
            eprintln!("{line}");
        }
        for pid in lifecycle.live_keepers() {
            eprintln!(
                "teardown still in progress under keeper pid {pid}; its outcome is unknown, \
                 and processes it started may still be running"
            );
        }
        // 128 + signal number, the conventional shell encoding.
        std::process::exit(exit_code);
    });
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
