//! Event pipe delivery and best-effort orphan outcome logging.

use std::os::fd::AsRawFd;
use std::time::Instant;

use super::super::protocol::{Event, Outcome};
use super::{Keeper, last_error};

impl Keeper {
    pub(super) fn finish(&mut self, outcome: &Outcome) {
        for proc_ in outcome.teardown.accounting().0 {
            self.queue(&Event::Killed(proc_.clone()));
        }
        self.queue(&Event::Done(Outcome {
            status: outcome.status,
            teardown: outcome
                .teardown
                .clone()
                .map_processes(|p| p.len(), |p| p.len()),
            interrupt_delivered: outcome.interrupt_delivered,
        }));
        let until = Instant::now() + self.spec.schedule.flush;
        while !self.pending.is_empty() && self.events.is_some() && Instant::now() < until {
            self.flush();
            if !self.pending.is_empty() {
                self.service(Some(until.saturating_duration_since(Instant::now())));
            }
        }
        if self.owner_gone || self.events.is_none() {
            self.log_orphaned(outcome);
        }
    }

    pub(super) fn queue(&mut self, event: &Event) {
        if let Ok(mut line) = serde_json::to_vec(event) {
            line.push(b'\n');
            self.pending.extend(line);
        }
        self.flush();
    }

    /// Write what the events pipe will take now. Backpressure leaves the rest
    /// queued; only a closed reader ends delivery.
    pub(super) fn flush(&mut self) {
        let Some(events) = &self.events else {
            self.pending.clear();
            return;
        };
        while !self.pending.is_empty() {
            let (head, _) = self.pending.as_slices();
            // SAFETY: writes from a slice we own.
            let n = unsafe {
                libc::write(
                    events.as_raw_fd(),
                    head.as_ptr().cast::<libc::c_void>(),
                    head.len(),
                )
            };
            match usize::try_from(n) {
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(_) => {
                    if last_error().kind() != std::io::ErrorKind::WouldBlock {
                        // EPIPE: nobody is reading, so `review` is gone.
                        self.events = None;
                        self.pending.clear();
                        self.owner_gone();
                    }
                    return;
                }
            }
        }
    }

    /// `review` is gone, so the outcome goes to the orphan log: one line, one
    /// write call. Best-effort - nothing reads it back.
    fn log_orphaned(&self, outcome: &Outcome) {
        let Some(path) = &self.spec.orphan_log else {
            return;
        };
        let survivors = outcome.teardown.named();
        let line = serde_json::json!({
            "epoch_secs": crate::provider::now_epoch_secs(),
            "label": self.spec.label,
            "keeper_pid": self.me,
            "outcome": outcome,
            "survivor_list": survivors,
        });
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            return;
        };
        bytes.push(b'\n');
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = (&file).write(&bytes);
        }
    }
}
