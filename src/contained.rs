//! Harness invocation containment. Command-owned bookkeeping lives in
//! `supervisor::Lifecycle`; every handle retains its exact owner through drop.

use crate::keeper::{self, Event, Outcome, Proc};
use crate::supervisor::{Control, Lifecycle};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::process::Child;

/// How a teardown is paced, and which binary is the keeper. Injectable so the
/// integration tests can run the real keeper in milliseconds.
#[derive(Clone, Debug)]
pub struct Settings {
    pub keeper: PathBuf,
    pub schedule: keeper::TeardownSchedule,
    pub startup: Duration,
    pub probe_timeout: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // The running `review` is its own keeper; `main` dispatches on
            // `keeper::ARG` before anything else. `/proc/self/exe` rather than
            // the path it was started from: exec'd in the forked child, it is
            // the image this `review` is running, so a binary replaced
            // mid-run (an install while runs are in flight) cannot hand later
            // launches a keeper speaking another version of the protocol.
            keeper: PathBuf::from("/proc/self/exe"),
            schedule: keeper::TeardownSchedule::default(),
            startup: crate::timings::KEEPER_STARTUP,
            probe_timeout: crate::timings::VERSION_PROBE,
        }
    }
}

impl Settings {
    /// The signal supervisor's waiting allowance after terminate. This covers
    /// either cleanup grace, not a guarantee against blocking kernel operations.
    pub fn terminate_bound(&self) -> Duration {
        self.schedule.worst_case(keeper::Start::Terminate)
    }
}

/// What one invocation left behind, as recorded in its audit and sidecar rows.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Leftovers {
    /// Ended by the teardown. Recorded, never printed: nobody can act on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub killed: Vec<Proc>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub killed_dropped: usize,
    /// Still alive after the teardown. Printed, and the command fails.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub survivors: Vec<Proc>,
    /// Enumeration may be incomplete; `survivors` is a lower bound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    /// The teardown's outcome never arrived intact (keeper died, events
    /// truncated or malformed, run abandoned before it finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown: Option<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl Leftovers {
    /// Keep existing append-only audit/sidecar rows readable: the historical
    /// flat schema is an explicit projection, never the keeper wire type. All
    /// unknown reasons remain in its one string; do not replace it with a tag.
    pub fn from_teardown(teardown: &keeper::Teardown) -> Self {
        let (killed, killed_dropped) = teardown.accounting();
        let (incomplete, unknown) = match teardown {
            keeper::Teardown::Clean { .. } => (false, None),
            keeper::Teardown::Survived { more_unnamed, .. } => (*more_unnamed, None),
            keeper::Teardown::Unknown { reasons, .. } => (false, Some(reasons.join("\n"))),
        };
        Self {
            killed: killed.to_vec(),
            killed_dropped,
            survivors: teardown.named().to_vec(),
            incomplete,
            unknown,
        }
    }

    pub fn teardown(self) -> keeper::Teardown {
        let Self {
            killed,
            killed_dropped: dropped,
            survivors: named,
            incomplete: more_unnamed,
            unknown,
        } = self;
        if let Some(why) = unknown {
            keeper::Teardown::Unknown {
                killed,
                dropped,
                named,
                reasons: vec![why],
            }
        } else if named.is_empty() && !more_unnamed {
            keeper::Teardown::Clean { killed, dropped }
        } else {
            keeper::Teardown::Survived {
                killed,
                dropped,
                named,
                more_unnamed,
            }
        }
    }

    /// Nothing of the invocation is alive, as established by its keeper.
    pub fn is_clean(&self) -> bool {
        self.survivors.is_empty() && !self.incomplete && self.unknown.is_none()
    }

    /// Nothing worth recording at all.
    pub fn is_empty(&self) -> bool {
        self.is_clean() && self.killed.is_empty() && self.killed_dropped == 0
    }

    /// Fold another invocation's leftovers into this record (grok wakes, the
    /// version probe), never losing a non-clean verdict.
    pub fn merge(&mut self, other: Leftovers) {
        let mut teardown = self.clone().teardown();
        teardown.merge(other.teardown());
        *self = Self::from_teardown(&teardown);
    }
}

/// What the events reader collected.
#[derive(Default)]
struct Events {
    survivors: Vec<Proc>,
    done: Option<Outcome<usize>>,
    killed: Vec<Proc>,
    damage: Vec<String>,
    terminal_reasons: Vec<String>,
}

impl Events {
    fn observe(&mut self, event: &Event) {
        if self.done.is_some() {
            self.damage
                .push("event after keeper terminal report".into());
        }
        match event {
            Event::Survivor(proc_) => self.survivors.push(proc_.clone()),
            Event::Killed(proc_) => self.killed.push(proc_.clone()),
            Event::Done(outcome) => {
                if let keeper::Teardown::Unknown { reasons, .. } = &outcome.teardown {
                    self.terminal_reasons.extend_from_slice(reasons);
                }
                if self.done.is_some() {
                    self.damage.push("duplicate keeper terminal report".into());
                } else {
                    self.done = Some(outcome.clone());
                }
            }
            Event::Spawned { .. } | Event::ExecFailed { .. } | Event::Exited { .. } => {}
        }
    }

    fn finished(&self, keeper_status: &std::io::Result<std::process::ExitStatus>) -> Finished {
        let mut reasons = self.damage.clone();
        match keeper_status {
            Err(e) => reasons.push(format!("could not wait for the keeper: {e}")),
            Ok(s) if !s.success() => reasons.push(format!("the keeper exited abnormally ({s})")),
            Ok(_) => {}
        }
        match &self.done {
            None => reasons.push("the keeper's report never arrived".into()),
            Some(outcome) => {
                let (killed, named, _) = outcome.teardown.process_counts();
                if killed != self.killed.len() || named != self.survivors.len() {
                    reasons.push("keeper process evidence did not match Done counts".into());
                }
                // Killed records cover different times and may reuse a PID;
                // only the final survivor snapshot has unique PID numbers.
                if self
                    .survivors
                    .iter()
                    .map(|p| p.pid)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != self.survivors.len()
                {
                    reasons.push("duplicate survivor identities in keeper report".into());
                }
                let expanded = outcome
                    .teardown
                    .clone()
                    .map_processes(|_| self.killed.clone(), |_| self.survivors.clone());
                if !expanded.valid() {
                    reasons.push("invalid keeper teardown verdict".into());
                }
            }
        }
        if reasons.is_empty() {
            let outcome = self.done.as_ref().expect("missing Done adds damage above");
            return Finished {
                status: decode_status(Some(outcome)),
                leftovers: Leftovers::from_teardown(
                    &outcome
                        .teardown
                        .clone()
                        .map_processes(|_| self.killed.clone(), |_| self.survivors.clone()),
                ),
                interrupt_delivered: Some(outcome.interrupt_delivered),
            };
        }
        reasons.extend_from_slice(&self.terminal_reasons);
        // Keep every streamed process even when the terminal never arrives.
        // Unknown makes clear that accounting and its dropped count are unproven.
        let dropped = self
            .done
            .as_ref()
            .map_or(0, |o| o.teardown.process_counts().2);
        let teardown = keeper::Teardown::Unknown {
            killed: self.killed.clone(),
            dropped,
            named: self.survivors.clone(),
            reasons,
        };
        Finished {
            status: decode_status(self.done.as_ref()),
            leftovers: Leftovers::from_teardown(&teardown),
            interrupt_delivered: None,
        }
    }
}

/// Maximum retained physical event line, including its newline. Unlike harness
/// output observers, keeper framing reports oversize, partial EOF and read errors
/// as transport damage; they can never be silently skipped or accepted as JSON.
const EVENT_LINE_MAX: usize = 64 << 10;

async fn read_events(
    fd: OwnedFd,
    spawned: tokio::sync::oneshot::Sender<std::result::Result<u32, String>>,
    exited: tokio::sync::watch::Sender<bool>,
    collected: Arc<Mutex<Events>>,
) {
    let rx = match tokio::net::unix::pipe::Receiver::from_owned_fd(fd) {
        Ok(rx) => rx,
        Err(e) => {
            collected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .damage
                .push(format!("could not read the keeper's events: {e}"));
            return;
        }
    };
    read_event_stream(tokio::io::BufReader::new(rx), spawned, exited, collected).await;
}

async fn read_event_stream<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    spawned: tokio::sync::oneshot::Sender<std::result::Result<u32, String>>,
    exited: tokio::sync::watch::Sender<bool>,
    collected: Arc<Mutex<Events>>,
) {
    let record = |f: &mut dyn FnMut(&mut Events)| {
        let mut events = collected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut events);
    };
    let mut spawned = Some(spawned);
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let available = match reader.fill_buf().await {
            Ok(bytes) => bytes,
            Err(e) => {
                record(&mut |events| events.damage.push(format!("keeper event read failed: {e}")));
                break;
            }
        };
        if available.is_empty() {
            if !line.is_empty() || oversized {
                record(&mut |events| events.damage.push("a truncated keeper event".into()));
            }
            break;
        }
        let end = available.iter().position(|b| *b == b'\n');
        let count = end.map_or(available.len(), |i| i + 1);
        if !oversized {
            if count > EVENT_LINE_MAX.saturating_sub(line.len()) {
                oversized = true;
                line.clear();
                record(&mut |events| events.damage.push("an oversized keeper event".into()));
            } else {
                line.extend_from_slice(&available[..count]);
            }
        }
        reader.consume(count);
        if end.is_none() {
            continue;
        }
        if !oversized {
            match serde_json::from_slice::<Event>(&line) {
                Ok(event) => {
                    record(&mut |events| events.observe(&event));
                    match event {
                        Event::Spawned { pid } => {
                            if let Some(tx) = spawned.take() {
                                let _ = tx.send(Ok(pid));
                            }
                        }
                        Event::ExecFailed { reason } => {
                            if let Some(tx) = spawned.take() {
                                let _ = tx.send(Err(reason));
                            }
                        }
                        Event::Exited { .. } => {
                            exited.send_replace(true);
                        }
                        Event::Survivor(_) | Event::Killed(_) | Event::Done(_) => {}
                    }
                }
                Err(e) => record(&mut |events| {
                    events.damage.push(format!("a malformed keeper event: {e}"));
                }),
            }
        }
        line.clear();
        oversized = false;
    }
}

/// A harness running under its keeper. Use it as the harness's `Child`:
/// take its pipes from `child`, wait with `wait`, then `finish`.
pub struct Contained {
    /// The keeper process. Its stdio is the harness's.
    pub child: Child,
    keeper_pid: u32,
    lifecycle: Lifecycle,
    control: Arc<Control>,
    events: Option<tokio::task::JoinHandle<()>>,
    collected: Arc<Mutex<Events>>,
    /// How long `finish` waits for the events pipe to close after the keeper
    /// has exited.
    flush_bound: Duration,
    spawned: Option<tokio::sync::oneshot::Receiver<std::result::Result<u32, String>>>,
    exited: tokio::sync::watch::Receiver<bool>,
    label: String,
    finished: bool,
}

/// Launch `harness` under a keeper. The keeper starts nothing until `go`, so
/// the caller can record its identity first. Harness stdout and stderr are
/// piped; stdin is the caller's choice.
///
/// Keeper admission is delegated to the command's Lifecycle, which serializes
/// refusal, spawn and registration with admission closure. There is no separate
/// containment stop flag. Admission is not harness execution: an admitted keeper
/// still awaits `go`, and remains owned if stop races with that later operation.
pub fn spawn(
    lifecycle: &Lifecycle,
    launch: keeper::Launch,
    stdin: std::process::Stdio,
    settings: &Settings,
    label: &str,
    orphan_log: Option<PathBuf>,
) -> Result<Contained> {
    let launcher = keeper::Launcher::new(
        &settings.keeper,
        launch,
        settings.schedule.clone(),
        settings.startup,
        label,
        orphan_log,
        stdin,
    )?;
    let keeper::Launcher {
        command,
        control: control_w,
        events: events_r,
        inherited,
    } = launcher;
    let mut cmd = tokio::process::Command::from(command);
    let control = Arc::new(Control(control_w));
    let child = lifecycle.spawn(&mut cmd, &control, settings.terminate_bound())?;
    drop(inherited);
    let keeper_pid = child.id().unwrap_or(0);
    let (tx, rx) = tokio::sync::oneshot::channel();
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(events_r.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            events_r.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        );
        let flags = libc::fcntl(control.0.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            control.0.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        );
    }
    let (exited_tx, exited_rx) = tokio::sync::watch::channel(false);
    let collected = Arc::new(Mutex::new(Events::default()));
    let events = tokio::spawn(read_events(events_r, tx, exited_tx, Arc::clone(&collected)));
    Ok(Contained {
        child,
        keeper_pid,
        lifecycle: lifecycle.clone(),
        control,
        events: Some(events),
        collected,
        flush_bound: settings.schedule.flush_bound(),
        spawned: Some(rx),
        exited: exited_rx,
        label: label.to_string(),
        finished: false,
    })
}

/// The one decode of the keeper's raw wait status, for both report branches.
fn decode_status(done: Option<&keeper::Outcome<usize>>) -> crate::process::Exit {
    done.and_then(|o| o.status).map_or(
        crate::process::Exit::Unknown,
        crate::process::Exit::from_wait_status,
    )
}

/// How a contained invocation ended.
pub struct Finished {
    /// The harness's own exit status. `Unknown` when it never ran, or its
    /// keeper's report did not arrive.
    pub status: crate::process::Exit,
    pub leftovers: Leftovers,
    /// Unknown if the terminal report could not be validated.
    pub interrupt_delivered: Option<bool>,
}

impl Contained {
    /// The keeper's pid: what the in-flight marker records and `review
    /// interrupt` signals.
    pub fn pid(&self) -> u32 {
        self.keeper_pid
    }

    /// Name the run in the ledger and the report, once its session is known.
    pub fn set_label(&mut self, label: String) {
        self.label = label;
    }

    /// Let the keeper start the harness, then wait for its launch
    /// acknowledgement (or failure). Returns the harness's pid. `Ok` does
    /// not prove the harness exec'd (see `keeper::Event::Spawned`); its
    /// wait status, from `wait`, does.
    pub async fn go(&mut self) -> Result<u32> {
        self.control
            .send(keeper::Control::Go.byte())
            .context("could not tell the keeper to launch")?;
        let rx = self
            .spawned
            .take()
            .context("the harness was already launched")?;
        match rx.await {
            Ok(Ok(pid)) => Ok(pid),
            Ok(Err(reason)) => anyhow::bail!("failed to launch: {reason}"),
            Err(_) => anyhow::bail!("the keeper exited before launching the harness"),
        }
    }

    /// `go`, keeping the teardown's verdict when the launch fails: the keeper
    /// still reports, and the ledger gets what it found rather than an
    /// "unknown" from an abandoned handle.
    pub async fn launch(mut self) -> Result<Self> {
        match self.go().await {
            Ok(_) => Ok(self),
            Err(e) => {
                let status = self.wait().await;
                self.finish(&status).await;
                Err(e)
            }
        }
    }

    /// Forward one SIGINT to the harness (the keeper ignores repeats).
    pub fn interrupt(&self) {
        let _ = self.control.send(keeper::Control::Interrupt.byte());
    }

    /// Tear the harness and everything it started down now.
    pub fn terminate(&self) {
        let _ = self.control.send(keeper::Control::Terminate.byte());
    }

    /// Wait for the harness itself to exit (or the keeper, should it die
    /// first). The run's teardown has not finished yet: follow with `wait`.
    /// Cancel-safe, so it can sit in a `select!` beside a watchdog, which must
    /// judge the harness and not the teardown of what it left.
    pub async fn wait_harness(&mut self) {
        let mut exited = self.exited.clone();
        tokio::select! {
            _ = exited.wait_for(|e| *e) => {}
            _ = self.child.wait() => {}
        }
    }

    /// Wait for the keeper to exit, which it does only after the teardown.
    /// Cancel-safe.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Collect the keeper's report once `wait` has returned, and record the
    /// verdict. An incomplete report is never read as a clean one.
    pub async fn finish(
        mut self,
        keeper_status: &std::io::Result<std::process::ExitStatus>,
    ) -> Finished {
        // `finished` is set only once the verdict is recorded: cancelled
        // before that, `Drop` records the run as unknown.
        //
        // Bounded: the keeper has exited, but a harness stuck before its
        // exec still holds the inherited events pipe, and EOF would never
        // come. What was read by then is the report.
        if let Some(handle) = self.events.as_mut() {
            match tokio::time::timeout(self.flush_bound, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => self
                    .collected
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .damage
                    .push(format!("keeper event reader task failed: {e}")),
                Err(_) => {
                    // Keep ownership through abort/join: cancellation at either
                    // await still lets Drop abort the retained reader.
                    self.events.as_ref().expect("retained reader").abort();
                    if let Err(e) = self.events.as_mut().expect("retained reader").await
                        && !e.is_cancelled()
                    {
                        self.collected
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .damage
                            .push(format!("keeper event reader task failed: {e}"));
                    }
                    self.collected
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .damage
                        .push("the keeper's report was cut off".into());
                }
            }
        }
        self.events.take();
        let events = self
            .collected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let finished = events.finished(keeper_status);
        drop(events);
        self.lifecycle.complete(
            &self.label,
            self.keeper_pid,
            &self.control,
            &finished.leftovers,
        );
        self.finished = true;
        finished
    }
}

impl Drop for Contained {
    /// A run abandoned before `finish` (an early error return, a cancelled
    /// task) still owes its teardown: the keeper is told to terminate, and the
    /// ledger records that nobody saw it finish.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.terminate();
        if let Some(reader) = &self.events {
            reader.abort();
        }
        let events = self
            .collected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let abandoned = Err(std::io::Error::other(
            "the run ended before its teardown was confirmed",
        ));
        let finished = events.finished(&abandoned);
        self.lifecycle.complete(
            &self.label,
            self.keeper_pid,
            &self.control,
            &finished.leftovers,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_(pid: u32, cmdline: &str) -> Proc {
        Proc {
            pid,
            cmdline: cmdline.to_string(),
            truncated: false,
        }
    }

    #[tokio::test]
    async fn framing_damage_and_duplicate_done_keep_all_known_evidence() {
        use std::os::unix::process::ExitStatusExt;
        let first = Event::Done(Outcome {
            status: Some(0),
            interrupt_delivered: true,
            teardown: keeper::Teardown::Clean {
                killed: 0,
                dropped: 0,
            },
        });
        let extra = proc_(42, "late worker");
        let second = Event::Done(Outcome {
            status: Some(0),
            interrupt_delivered: false,
            teardown: keeper::Teardown::Unknown {
                killed: 0,
                dropped: 0,
                named: 1,
                reasons: vec!["second report uncertainty".into()],
            },
        });
        let mut bytes = serde_json::to_vec(&first).expect("first");
        bytes.push(b'\n');
        bytes.extend(vec![b'x'; EVENT_LINE_MAX + 50]);
        bytes.extend(b"\nnot json\n");
        bytes.extend(serde_json::to_vec(&Event::Survivor(extra.clone())).expect("survivor"));
        bytes.push(b'\n');
        bytes.extend(serde_json::to_vec(&second).expect("second"));
        bytes.push(b'\n');
        bytes.extend(b"partial");
        let collected = Arc::new(Mutex::new(Events::default()));
        let (spawned, _) = tokio::sync::oneshot::channel();
        let (exited, _) = tokio::sync::watch::channel(false);
        read_event_stream(
            tokio::io::BufReader::with_capacity(7, bytes.as_slice()),
            spawned,
            exited,
            Arc::clone(&collected),
        )
        .await;
        let events = collected.lock().expect("events");
        let result = events.finished(&Ok(std::process::ExitStatus::from_raw(0)));
        let why = result.leftovers.unknown.expect("unknown");
        for expected in [
            "oversized",
            "malformed",
            "duplicate",
            "truncated",
            "second report uncertainty",
        ] {
            assert!(why.contains(expected), "{why}");
        }
        assert_eq!(result.interrupt_delivered, None);
        assert_eq!(result.leftovers.survivors, vec![extra]);
    }

    #[tokio::test]
    async fn compact_done_accepts_unbounded_survivor_stream_and_reused_killed_pids() {
        use std::os::unix::process::ExitStatusExt;
        let mut bytes = Vec::new();
        for pid in 1..=600 {
            let mut proc_ = proc_(pid, &"x".repeat(512));
            proc_.truncated = true;
            let event = Event::Survivor(proc_);
            let line = serde_json::to_vec(&event).expect("survivor");
            assert!(line.len() < EVENT_LINE_MAX);
            bytes.extend(line);
            bytes.push(b'\n');
        }
        // Accounting spans time: both killed records may share a PID with
        // each other and with a survivor after reuse. Do not reject that.
        for _ in 0..2 {
            bytes.extend(
                serde_json::to_vec(&Event::Killed(proc_(1, "earlier worker"))).expect("killed"),
            );
            bytes.push(b'\n');
        }
        let done = Event::Done(Outcome {
            status: Some(0),
            interrupt_delivered: true,
            teardown: keeper::Teardown::Survived {
                killed: 2,
                dropped: 3,
                named: 600,
                more_unnamed: true,
            },
        });
        let terminal = serde_json::to_vec(&done).expect("done");
        assert!(terminal.len() < 1024);
        bytes.extend(terminal);
        bytes.push(b'\n');
        assert!(bytes.len() > EVENT_LINE_MAX * 4);
        let collected = Arc::new(Mutex::new(Events::default()));
        let (spawned, _) = tokio::sync::oneshot::channel();
        let (exited, _) = tokio::sync::watch::channel(false);
        read_event_stream(
            tokio::io::BufReader::with_capacity(37, bytes.as_slice()),
            spawned,
            exited,
            Arc::clone(&collected),
        )
        .await;
        let mut events = collected.lock().expect("events");
        let result = events.finished(&Ok(std::process::ExitStatus::from_raw(0)));
        assert_eq!(result.interrupt_delivered, Some(true));
        assert!(result.leftovers.unknown.is_none());
        assert_eq!(result.leftovers.survivors.len(), 600);
        assert_eq!(result.leftovers.killed.len(), 2);
        assert_eq!(result.leftovers.killed_dropped, 3);
        events.done = None;
        let damaged = events.finished(&Ok(std::process::ExitStatus::from_raw(0)));
        assert!(damaged.leftovers.unknown.is_some());
        assert_eq!(damaged.leftovers.killed.len(), 2);
        assert_eq!(damaged.leftovers.survivors.len(), 600);
    }

    struct FailingReader {
        bytes: Vec<u8>,
        offset: usize,
    }
    impl tokio::io::AsyncRead for FailingReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if this.offset == this.bytes.len() {
                return std::task::Poll::Ready(Err(std::io::Error::other("test read failure")));
            }
            let n = buf.remaining().min(this.bytes.len() - this.offset);
            buf.put_slice(&this.bytes[this.offset..this.offset + n]);
            this.offset += n;
            std::task::Poll::Ready(Ok(()))
        }
    }
    impl tokio::io::AsyncBufRead for FailingReader {
        fn poll_fill_buf(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<&[u8]>> {
            let this = self.get_mut();
            if this.offset == this.bytes.len() {
                return std::task::Poll::Ready(Err(std::io::Error::other("test read failure")));
            }
            std::task::Poll::Ready(Ok(&this.bytes[this.offset..]))
        }
        fn consume(self: std::pin::Pin<&mut Self>, amount: usize) {
            self.get_mut().offset += amount;
        }
    }

    #[tokio::test]
    async fn io_failure_after_done_is_not_successful_eof() {
        use std::os::unix::process::ExitStatusExt;
        let done = Event::Done(Outcome {
            status: Some(0),
            interrupt_delivered: true,
            teardown: keeper::Teardown::Clean {
                killed: 0,
                dropped: 0,
            },
        });
        let mut bytes = serde_json::to_vec(&done).expect("done");
        bytes.push(b'\n');
        let collected = Arc::new(Mutex::new(Events::default()));
        let (spawned, _) = tokio::sync::oneshot::channel();
        let (exited, _) = tokio::sync::watch::channel(false);
        read_event_stream(
            FailingReader { bytes, offset: 0 },
            spawned,
            exited,
            Arc::clone(&collected),
        )
        .await;
        let result = collected
            .lock()
            .expect("events")
            .finished(&Ok(std::process::ExitStatus::from_raw(0)));
        assert!(
            result
                .leftovers
                .unknown
                .expect("unknown")
                .contains("test read failure")
        );
        assert_eq!(result.interrupt_delivered, None);
    }

    #[test]
    fn terminal_validation_preserves_evidence_and_never_invents_cleanliness() {
        use std::os::unix::process::ExitStatusExt;
        let keeper_ok = Ok(std::process::ExitStatus::from_raw(0));
        let survivor = proc_(7, "worker");
        let mut events = Events {
            survivors: vec![survivor.clone()],
            done: Some(Outcome {
                status: Some(0),
                interrupt_delivered: true,
                teardown: keeper::Teardown::Survived {
                    killed: 0,
                    dropped: 0,
                    named: 1,
                    more_unnamed: false,
                },
            }),
            ..Events::default()
        };
        let valid = events.finished(&keeper_ok);
        assert_eq!(valid.interrupt_delivered, Some(true));
        assert_eq!(valid.leftovers.survivors, vec![survivor.clone()]);
        events.survivors.clear();
        let missing = events.finished(&keeper_ok);
        assert!(missing.leftovers.unknown.is_some());
        assert_eq!(missing.interrupt_delivered, None);
        assert!(
            missing.leftovers.survivors.is_empty(),
            "counts cannot invent missing identity evidence"
        );
        events.done.as_mut().expect("done").teardown = keeper::Teardown::Clean {
            killed: 0,
            dropped: 0,
        };
        events.survivors.push(survivor.clone());
        assert!(events.finished(&keeper_ok).leftovers.unknown.is_some());
        events.survivors.clear();
        let dead = Ok(std::process::ExitStatus::from_raw(libc::SIGKILL));
        assert!(events.finished(&dead).leftovers.unknown.is_some());
        events.done.as_mut().expect("done").teardown = keeper::Teardown::Survived {
            killed: 0,
            dropped: 0,
            named: 0,
            more_unnamed: false,
        };
        assert!(events.finished(&keeper_ok).leftovers.unknown.is_some());
        events.done.as_mut().expect("done").teardown = keeper::Teardown::Survived {
            killed: 0,
            dropped: 0,
            named: 2,
            more_unnamed: false,
        };
        events.survivors = vec![survivor.clone(), survivor];
        assert!(events.finished(&keeper_ok).leftovers.unknown.is_some());
        events.done = None;
        assert!(events.finished(&keeper_ok).leftovers.unknown.is_some());
    }

    #[test]
    fn merging_unknowns_preserves_every_reason_and_historical_rows_read() {
        let mut first: Leftovers =
            serde_json::from_str(r#"{"unknown":"probe timed out","survivors":[]}"#)
                .expect("historical row");
        first.merge(Leftovers {
            unknown: Some("wake keeper died".into()),
            ..Leftovers::default()
        });
        let why = first.unknown.as_deref().expect("unknown");
        assert!(why.contains("probe timed out") && why.contains("wake keeper died"));
        let roundtrip: Leftovers =
            serde_json::from_str(&serde_json::to_string(&first).expect("serialize")).expect("read");
        assert_eq!(first, roundtrip);
        assert!(!roundtrip.is_clean());
    }

    #[test]
    fn merging_keeps_a_non_clean_verdict() {
        let mut run = Leftovers {
            killed: vec![proc_(1, "a")],
            ..Leftovers::default()
        };
        run.merge(Leftovers {
            survivors: vec![proc_(2, "b")],
            ..Leftovers::default()
        });
        run.merge(Leftovers::default());
        assert!(!run.is_clean());
        assert_eq!(run.killed.len(), 1);
        assert_eq!(run.survivors.len(), 1);
    }
}
