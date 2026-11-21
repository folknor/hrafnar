//! Event pipe delivery and best-effort orphan outcome logging.

use std::os::fd::AsRawFd;
use std::time::Instant;

use super::super::protocol::{Event, Outcome, Proc};
use super::{Keeper, last_error};

/// Shorter trailing fragments are left alone: they could be any text.
const SECRET_FRAGMENT_MIN: usize = 8;

/// `cmdline` with every occurrence of `secret` replaced, and with a trailing
/// partial occurrence replaced too: a command line cut at its length limit
/// can end part-way through a secret, which a whole-value match misses.
fn redact(cmdline: &str, secret: &str) -> String {
    let mut out = cmdline.replace(secret, "<redacted>");
    let mut cut = secret.len().saturating_sub(1);
    while cut >= SECRET_FRAGMENT_MIN {
        if secret.is_char_boundary(cut) && out.ends_with(&secret[..cut]) {
            out.truncate(out.len() - cut);
            out.push_str("<redacted>");
            break;
        }
        cut -= 1;
    }
    out
}

impl Keeper {
    /// One process record with the child's secret environment values - those
    /// whose names look secret (`incident::looks_secret`) and are long enough
    /// to be credentials - removed from its command line. Every process
    /// record leaving the keeper goes through this.
    pub(super) fn scrub_proc(&self, mut proc_: Proc) -> Proc {
        for (key, value) in &self.spec.launch.env {
            if value.len() >= 16 && crate::incident::looks_secret(&String::from_utf8_lossy(key)) {
                proc_.cmdline = redact(&proc_.cmdline, &String::from_utf8_lossy(value));
            }
        }
        proc_
    }

    /// The outcome with the child's secret environment values removed from
    /// every recorded command line. A hook or wrapper can pass a credential
    /// `review` injected (a `claude_token_command` token) to a process as an
    /// argument; both the event stream and the orphan log, which is written
    /// with nobody left to scrub it, take this form.
    fn scrubbed(&self, outcome: &Outcome) -> Outcome {
        let scrub = |procs: Vec<Proc>| procs.into_iter().map(|p| self.scrub_proc(p)).collect();
        Outcome {
            status: outcome.status,
            teardown: outcome.teardown.clone().map_processes(scrub, scrub),
            interrupt_delivered: outcome.interrupt_delivered,
        }
    }

    pub(super) fn finish(&mut self, outcome: &Outcome) {
        let outcome = &self.scrubbed(outcome);
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

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn a_whole_or_cut_off_secret_is_redacted() {
        let secret = "sk-ant-oat01-0123456789abcdefghij";
        assert_eq!(
            redact(&format!("x --t {secret} y"), secret),
            "x --t <redacted> y"
        );
        // Cut at the command-line limit part-way through the secret.
        assert_eq!(
            redact("x --t sk-ant-oat01-0123", secret),
            "x --t <redacted>"
        );
        // A short coincidental tail is not taken for one.
        assert_eq!(redact("x sk-an", secret), "x sk-an");
    }
}
