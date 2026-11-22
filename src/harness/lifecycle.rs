//! The run lifecycle every harness adapter drives: one place owning the
//! ordering, so no runner restates it by hand.
//!
//! An adapter keeps its own control flow - argv, its stream readers, codex's
//! rollout baseline and recovery, grok's wake loop, classification - and calls
//! into this module for the steps whose order is load-bearing:
//!
//! 1. `ProcessSpec`: the exact process to launch, environment layered
//!    explicitly (adapter defaults, then the profile, then adapter-forced
//!    values and `PWD`), and the same value the incident bundle replays.
//! 2. `Run::spawn`: a stale interrupt request is cleared for a resume, a stop
//!    already requested refuses the launch, the keeper is spawned and recorded
//!    on the in-flight marker before anything launches.
//! 3. `Run::launch`: the keeper starts the harness; the launch signal fires
//!    (the marker for a known session is already up, which `review message`'s
//!    launch lock relies on); the prompt writer starts.
//! 4. `Run::wait`: harness exit, an optional adapter kill arm (codex's
//!    watchdog) and the graceful-stop arm, then the keeper's teardown with the
//!    stop arm still live.
//! 5. `Run::finish`: the exact interrupt request is taken before markers become
//!    handoff pending and transfer to the invocation reservation; the keeper's
//!    report is collected and the prompt writer is joined.
//!
//! What the readers observe (readiness to take a SIGINT, a session id learned
//! mid-stream) are facts on the `Lease`, kept apart from the marker they also
//! update: a fact arriving after the marker closed still counts for
//! classification, but never puts a marker back up.

use super::{Harness, HarnessCommand, HarnessRuntime, LaunchSignal, SessionStart};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What the harness reads on stdin.
pub enum Stdin {
    /// The prompt, written by a task the run owns.
    Pipe(Vec<u8>),
    Null,
}

/// The process an adapter launches: program, full argv (the command prefix's
/// leading arguments included), working directory and the environment
/// overrides applied on top of `review`'s own environment.
///
/// The overrides are layered in one fixed order, because the order is policy:
/// adapter defaults first, so a profile can restate them (`RUST_BACKTRACE`,
/// claude's and grok's command budgets); then the profile; then adapter-forced
/// values a profile must not undo (isolation a harness needs, which no current
/// adapter has); and `PWD` last, set to the working directory for every
/// harness, because a process that takes its directory from `PWD` rather than
/// from its cwd would otherwise run wherever `review` was started.
///
/// `env` is the effective overrides, not the complete environment: the child
/// also inherits whatever `review` inherited.
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub stdin: Stdin,
}

impl ProcessSpec {
    pub fn new(
        command: &HarnessCommand,
        args: impl IntoIterator<Item = String>,
        cwd: &Path,
        defaults: &[(&str, String)],
        profile: Option<&BTreeMap<String, String>>,
        forced: &[(&str, String)],
        stdin: Stdin,
    ) -> Self {
        let mut env = BTreeMap::new();
        for (key, value) in defaults {
            env.insert((*key).to_string(), value.clone());
        }
        if let Some(profile) = profile {
            env.extend(profile.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        for (key, value) in forced {
            env.insert((*key).to_string(), value.clone());
        }
        env.insert("PWD".to_string(), cwd.to_string_lossy().into_owned());
        Self {
            program: command.program.clone(),
            args: command.leading_args.iter().cloned().chain(args).collect(),
            cwd: cwd.to_path_buf(),
            env,
            stdin,
        }
    }

    fn launch_spec(&self) -> std::io::Result<crate::keeper::Launch> {
        crate::keeper::Launch::new(
            std::ffi::OsStr::new(&self.program),
            self.args.iter().map(std::ffi::OsString::from),
            &self.cwd,
            crate::keeper::Environment::Inherit(
                self.env
                    .iter()
                    .map(|(k, v)| (k.into(), Some(v.into())))
                    .collect(),
            ),
        )
    }
}

/// The in-flight marker state behind a `Lease`.
struct LeaseState {
    /// Every marker this lease holds: the session's, and for grok an echoed
    /// alias. One Guard per checked id exclusively owns its path until the
    /// same reservation takes cleanup authority for the pending handoff.
    guards: Vec<(crate::harness::SessionId, crate::inflight::Guard)>,
    reservation: Option<std::sync::Arc<crate::inflight::Reservation>>,
    admission_error: Option<String>,
    closed: bool,
    keeper: Option<u32>,
}

struct LeaseInner {
    harness: Harness,
    project: String,
    data: crate::data::DataDir,
    run_id: crate::sessions::handoff::RunId,
    predecessor: Option<crate::sessions::handoff::RunId>,
    state: Mutex<LeaseState>,
    ready: tokio::sync::watch::Sender<bool>,
    // Raw protocol evidence is deliberately not SessionId: a malformed first
    // event must survive classification while gaining no path authority.
    session: tokio::sync::watch::Sender<Option<String>>,
}

/// A run's in-flight marker and the facts its readers publish. Cloneable: the
/// readers hold clones to publish, the adapter to read the facts back after
/// the readers are collected. Only a `LeaseOwner` closes it.
#[derive(Clone)]
pub struct Lease(Arc<LeaseInner>);

/// Closes its lease when dropped, on every path: a normal finish, an early
/// return, a cancelled future. Not cloneable, so closing follows ownership.
pub struct LeaseOwner(Lease);

impl Drop for LeaseOwner {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl std::ops::Deref for LeaseOwner {
    type Target = Lease;
    fn deref(&self) -> &Lease {
        &self.0
    }
}

impl Lease {
    fn open(
        harness: Harness,
        project: &Path,
        data: &crate::data::DataDir,
        run_id: &crate::sessions::handoff::RunId,
        reserved: Option<std::sync::Arc<crate::inflight::Reservation>>,
        predecessor: Option<&crate::sessions::handoff::RunId>,
    ) -> Lease {
        let (reservation, admission_error) = if let Some(reserved) = reserved {
            (Some(reserved), None)
        } else {
            match crate::inflight::reserve(data, run_id) {
                Ok(reservation) => (reservation.map(std::sync::Arc::new), None),
                Err(e) => (None, Some(e.to_string())),
            }
        };
        Lease(Arc::new(LeaseInner {
            harness,
            project: project.to_string_lossy().into_owned(),
            data: data.clone(),
            run_id: run_id.clone(),
            predecessor: predecessor.cloned(),
            state: Mutex::new(LeaseState {
                guards: Vec::new(),
                reservation,
                admission_error,
                closed: false,
                keeper: None,
            }),
            ready: tokio::sync::watch::channel(false).0,
            session: tokio::sync::watch::channel(None).0,
        }))
    }

    /// A run whose session id is known before launch - every claude and grok
    /// run, every resume. The marker goes up now, before the spawn.
    pub fn known(
        session_id: &crate::harness::SessionId,
        harness: Harness,
        project: &Path,
        data: &crate::data::DataDir,
        run_id: &crate::sessions::handoff::RunId,
        reserved: Option<std::sync::Arc<crate::inflight::Reservation>>,
        predecessor: Option<&crate::sessions::handoff::RunId>,
    ) -> LeaseOwner {
        let lease = Self::open(harness, project, data, run_id, reserved, predecessor);
        if let Ok(mut state) = lease.0.state.lock() {
            lease
                .0
                .session
                .send_replace(Some(session_id.as_str().to_owned()));
            if state.admission_error.is_some() {
                return LeaseOwner(lease.clone());
            }
            let guard = lease.marker(session_id, state.keeper, state.reservation.as_deref());
            state.guards.push((session_id.clone(), guard));
        }
        LeaseOwner(lease)
    }

    /// A run that learns its session id from its stream (a fresh codex run).
    /// The marker goes up when `set_session` is first called.
    pub fn learned(
        harness: Harness,
        project: &Path,
        data: &crate::data::DataDir,
        run_id: &crate::sessions::handoff::RunId,
        reserved: Option<std::sync::Arc<crate::inflight::Reservation>>,
        predecessor: Option<&crate::sessions::handoff::RunId>,
    ) -> LeaseOwner {
        LeaseOwner(Self::open(
            harness,
            project,
            data,
            run_id,
            reserved,
            predecessor,
        ))
    }

    fn marker(
        &self,
        id: &crate::harness::SessionId,
        keeper: Option<u32>,
        reservation: Option<&crate::inflight::Reservation>,
    ) -> crate::inflight::Guard {
        let mut guard = crate::inflight::mark_for(
            id,
            self.0.harness,
            &self.0.project,
            &self.0.data,
            &self.0.run_id,
            self.0.predecessor.as_ref(),
        );
        if let Some(keeper) = keeper {
            guard.record_keeper(keeper);
            guard.record_ready(*self.0.ready.borrow() && self.offers_pid());
        }
        if let Some(reservation) = reservation {
            reservation.register(&guard);
        }
        guard
    }

    /// Whether this harness's pid is ever offered on the marker. Only an
    /// interruptible harness's turn can be ended by `review interrupt`.
    fn offers_pid(&self) -> bool {
        self.0.harness.interruptible()
    }

    fn record_keeper(&self, keeper: u32) {
        if let Ok(mut state) = self.0.state.lock() {
            state.keeper = Some(keeper);
            if state.closed {
                return;
            }
            for (_, guard) in &mut state.guards {
                guard.record_keeper(keeper);
                guard.record_ready(*self.0.ready.borrow() && self.offers_pid());
            }
        }
    }

    /// The harness has shown it can take a SIGINT and end its turn cleanly.
    /// Codex: its first stdout byte; claude: its `init` event.
    pub fn ready(&self) {
        // The fact first, so a `set_session` racing this under the lock sees it.
        self.0.ready.send_replace(true);
        let Ok(mut state) = self.0.state.lock() else {
            return;
        };
        if state.closed || !self.offers_pid() {
            return;
        }
        let keeper = state.keeper;
        for (_, guard) in &mut state.guards {
            guard.record_ready(keeper.is_some());
        }
    }

    /// The session id the stream reported. Accepted once: a repeat is
    /// harmless, a conflicting second id is ignored, so the marker, the
    /// interrupt request and the watchdog all stay on the first.
    pub fn set_session(&self, session_id: &str) {
        let Ok(mut state) = self.0.state.lock() else {
            return;
        };
        if self.0.session.borrow().is_some() {
            return;
        }
        self.0.session.send_replace(Some(session_id.to_string()));
        if state.closed || state.admission_error.is_some() || !state.guards.is_empty() {
            return;
        }
        let Ok(id) = crate::harness::SessionId::parse(session_id) else {
            return;
        };
        let guard = self.marker(&id, state.keeper, state.reservation.as_deref());
        state.guards.push((id, guard));
    }

    /// Hold a marker for a second id the run also answers to (grok echoing a
    /// session id other than the one it was given), kept current with the
    /// keeper like the first so a dead owner still shows cleanup pending.
    pub fn alias(&self, session_id: &str) {
        let Ok(mut state) = self.0.state.lock() else {
            return;
        };
        if state.closed
            || state.admission_error.is_some()
            || state.guards.iter().any(|(id, _)| id.as_str() == session_id)
        {
            return;
        }
        let Ok(id) = crate::harness::SessionId::parse(session_id) else {
            return;
        };
        let guard = self.marker(&id, state.keeper, state.reservation.as_deref());
        state.guards.push((id, guard));
    }

    /// Consume the interrupt request aimed at the session this lease holds
    /// and, when `close`, remove every marker - one transition under the
    /// lease lock, so a session discovered mid-settlement cannot put up a
    /// marker whose request this run then never consumes. The request goes
    /// before the markers; exact requests arriving later are harmless residue.
    fn settle(&self, close: bool) -> bool {
        let Ok(mut state) = self.0.state.lock() else {
            return false;
        };
        // Exact run requests cannot mark a later invocation. The legacy
        // session request is a deliberately weaker compatibility facade.
        let exact = state.guards.iter().fold(false, |any, (_, guard)| {
            guard.take_request(&self.0.data) || any
        });
        let requested = self
            .session()
            .as_deref()
            .is_some_and(|sid| crate::inflight::take_interrupt_request_in(sid, &self.0.data));
        let requested = exact || requested;
        if close {
            state.closed = true;
            for (_, guard) in &mut state.guards {
                guard.handoff();
            }
            let guards = std::mem::take(&mut state.guards);
            let reservation = state.reservation.take();
            drop(state);
            drop(guards);
            drop(reservation);
        }
        requested
    }

    /// Remove every marker. Facts still update afterwards; markers never do.
    fn close(&self) {
        let (guards, reservation) = match self.0.state.lock() {
            Ok(mut state) => {
                state.closed = true;
                for (_, guard) in &mut state.guards {
                    guard.handoff();
                }
                (std::mem::take(&mut state.guards), state.reservation.take())
            }
            Err(_) => return,
        };
        drop(guards);
        drop(reservation);
    }

    pub fn is_ready(&self) -> bool {
        *self.0.ready.borrow()
    }

    pub fn session(&self) -> Option<String> {
        self.0.session.borrow().clone()
    }

    /// Follows the session fact, for a watcher that needs the id the moment
    /// it is known (codex's watchdog).
    pub fn session_watch(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.0.session.subscribe()
    }
}

/// Who closes the lease a run uses.
pub enum LeaseUse {
    /// The run closes it in `finish` (or when dropped).
    Owned(LeaseOwner),
    /// An outer owner spanning several runs closes it (grok's wake turns).
    Shared(Lease),
}

/// The pipes an adapter reads. Taken before launch, so readers are running by
/// the time the harness writes.
pub struct Pipes {
    pub stdout: tokio::process::ChildStdout,
    pub stderr: tokio::process::ChildStderr,
}

/// One harness process under its keeper, from spawn to teardown.
pub struct Run {
    harness: Harness,
    contained: Option<crate::contained::Contained>,
    lease: Lease,
    owner: Option<LeaseOwner>,
    stdin: Option<(tokio::process::ChildStdin, Vec<u8>)>,
    writer: Option<tokio::task::JoinHandle<Result<()>>>,
    readers: Vec<tokio::task::AbortHandle>,
    stop: tokio::sync::watch::Receiver<bool>,
    stopped_by_signal: bool,
    keeper: Option<std::io::Result<std::process::ExitStatus>>,
}

/// How a run ended, as far as the lifecycle knows.
pub struct Ended {
    /// The harness's own exit status, `Unknown` when the keeper could not
    /// report it. Forensics only: no adapter classifies by it.
    pub status: crate::process::Exit,
    pub leftovers: crate::contained::Leftovers,
    /// An operator interrupt request was consumed, or `review` was signalled
    /// and ended the turn. Whether it counts is the outcome's call.
    pub interrupt_requested: bool,
    pub stopped_by_signal: bool,
    /// Writing the prompt failed (`Ok` with no stdin to write).
    pub stdin: Result<()>,
    /// The run's readers, aborted when this is dropped. The adapter keeps
    /// `Ended` alive while it collects them, so a cancellation during the
    /// drain cannot leave a reader detached. Held for its `Drop` alone.
    #[allow(dead_code)]
    pub readers: AbortOnDrop,
}

/// Aborts the tasks it holds when dropped; a finished task is unaffected.
pub struct AbortOnDrop(Vec<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

fn label(harness: Harness, session: Option<&str>) -> String {
    match session {
        Some(id) => format!("the {harness} run of session {id}"),
        None => format!("a {harness} run"),
    }
}

impl Run {
    /// Spawn the keeper for `spec`, without launching the harness yet.
    pub fn spawn(
        harness: Harness,
        spec: &ProcessSpec,
        session: &SessionStart,
        lease: LeaseUse,
        runtime: &HarnessRuntime,
    ) -> Result<(Run, Pipes)> {
        let (lease, owner) = match lease {
            LeaseUse::Owned(owner) => (owner.0.clone(), Some(owner)),
            LeaseUse::Shared(lease) => (lease, None),
        };
        {
            let state = lease
                .0
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("lease poisoned"))?;
            if let Some(reason) = &state.admission_error {
                anyhow::bail!("not launched: cannot reserve invocation markers: {reason}");
            }
            if state.guards.iter().any(|(_, g)| g.collision()) {
                anyhow::bail!("not launched: invocation marker path already exists");
            }
        }
        // A request left over from an earlier run of this session (its
        // `review` killed before consuming it) is not aimed at this one, and
        // would mark this run as interrupted. A fresh session is new.
        if let SessionStart::Resume(id) = session {
            let _ = crate::inflight::take_interrupt_request_in(id.as_str(), &runtime.data);
        }
        // After lease checks and stale-request consumption, avoid remaining
        // launch preparation on an observed graceful stop, preserving its
        // diagnostic over subsequent preparation failures. This snapshot is
        // not admission authority: contained delegates to Lifecycle::spawn's
        // locked gate, which closes the race. An already admitted keeper may
        // still reach harness `go` after stop; wait/escalation handle that run.
        if runtime.lifecycle.stop_requested() {
            anyhow::bail!("not launched: review was signalled to stop");
        }
        let stdin_cfg = match spec.stdin {
            Stdin::Pipe(_) => std::process::Stdio::piped(),
            Stdin::Null => std::process::Stdio::null(),
        };
        let mut contained = crate::contained::spawn(
            &runtime.lifecycle,
            spec.launch_spec()?,
            stdin_cfg,
            &runtime.teardown,
            &label(harness, lease.session().as_deref()),
            runtime.data.orphan_log(runtime.log_family),
        )?;
        // Before `go`, so a marker whose owner dies mid-run says who is
        // cleaning up.
        lease.record_keeper(contained.pid());
        let stdin = match &spec.stdin {
            Stdin::Pipe(bytes) => Some((
                contained
                    .child
                    .stdin
                    .take()
                    .with_context(|| format!("failed to open {harness} stdin"))?,
                bytes.clone(),
            )),
            Stdin::Null => None,
        };
        let stdout = contained
            .child
            .stdout
            .take()
            .with_context(|| format!("failed to open {harness} stdout"))?;
        let stderr = contained
            .child
            .stderr
            .take()
            .with_context(|| format!("failed to open {harness} stderr"))?;
        let run = Run {
            harness,
            contained: Some(contained),
            lease,
            owner,
            stdin,
            writer: None,
            readers: Vec::new(),
            stop: runtime.lifecycle.subscribe(),
            stopped_by_signal: false,
            keeper: None,
        };
        Ok((run, Pipes { stdout, stderr }))
    }

    /// Abort `reader` if the run is abandoned. The adapter still collects it
    /// with `collect_reader` on the normal path.
    pub fn track<T>(&mut self, reader: &tokio::task::JoinHandle<T>) {
        self.readers.push(reader.abort_handle());
    }

    /// Let the keeper start the harness, then tell the caller it may release
    /// the global lock, then start writing the prompt. Dropping `launched`
    /// unsent (a failed launch) tells it the same thing.
    pub async fn launch(&mut self, launched: Option<LaunchSignal>) -> Result<()> {
        let contained = self
            .contained
            .take()
            .context("the run was already launched")?;
        // Cancelled here, the future drops `contained` (whose `Drop` tears
        // down and records an unknown verdict) and then this `Run` (closing an
        // owned lease).
        self.contained = Some(contained.launch().await?);
        if let Some(signal) = launched {
            let _ = signal.send(());
        }
        if let Some((stdin, bytes)) = self.stdin.take() {
            self.writer = Some(tokio::spawn(super::write_stdin(stdin, bytes)));
        }
        Ok(())
    }

    /// The graceful stop for `review` being signalled: an interruptible
    /// harness that has shown it can take a SIGINT gets one and ends its turn
    /// with its session recorded; before that a SIGINT could kill it with
    /// nothing written (claude without a handler, codex's node wrapper
    /// orphaning the native binary), so the run is torn down instead.
    /// Harnesses that cannot be interrupted have no arm: the signal
    /// supervisor terminates their keepers after the graceful phase.
    fn stop_now(contained: &crate::contained::Contained, lease: &Lease) {
        if lease.is_ready() {
            contained.interrupt();
        } else {
            contained.terminate();
        }
    }

    /// Wait for the harness, then for its keeper's teardown. `kill` is the
    /// adapter's own reason to end the run (codex's watchdog): when it
    /// resolves the run is terminated, the arm disabled, and its value
    /// returned once the keeper is reaped. Pass `std::future::pending()` for
    /// none.
    pub async fn wait<T>(&mut self, kill: impl std::future::Future<Output = T>) -> Option<T> {
        let Run {
            harness,
            contained,
            lease,
            stop,
            stopped_by_signal,
            keeper,
            ..
        } = self;
        let contained = contained
            .as_mut()
            .expect("wait follows a successful launch");
        let stop_arm = harness.interruptible();
        tokio::pin!(kill);
        let mut killed = None;
        // Phase 1: the harness. A watchdog judges the harness, never the
        // teardown of what it left, so the kill arm ends with this phase.
        loop {
            tokio::select! {
                () = contained.wait_harness() => break,
                value = &mut kill, if killed.is_none() => {
                    killed = Some(value);
                    contained.terminate();
                }
                // An `Err` (no sender left, so no stop can ever come) does
                // not match, and select disables the arm.
                Ok(_) = stop.wait_for(|s| *s), if stop_arm && !*stopped_by_signal => {
                    *stopped_by_signal = true;
                    Self::stop_now(contained, lease);
                }
            }
        }
        // Phase 2: the keeper's teardown. The session still reads as in
        // flight until nothing of the run is left, and a stop arriving now
        // still counts.
        let status = loop {
            tokio::select! {
                status = contained.wait() => break status,
                Ok(_) = stop.wait_for(|s| *s), if stop_arm && !*stopped_by_signal => {
                    *stopped_by_signal = true;
                    Self::stop_now(contained, lease);
                }
            }
        };
        *keeper = Some(status);
        killed
    }

    /// Settle the run: take the interrupt request, remove the marker (an
    /// owned lease), collect the keeper's report, join the prompt writer.
    /// The lease's facts stay readable afterwards, and keep updating while
    /// the adapter drains its readers.
    pub async fn finish(mut self) -> Ended {
        if self.keeper.is_none() {
            self.wait(std::future::pending::<()>()).await;
        }
        let keeper = self.keeper.take().expect("waited above");
        // Consumed whatever became of the run, before handoff transfer; one
        // left behind would mark the session's next run as interrupted.
        let requested = self.lease.settle(self.owner.is_some());
        let session = self.lease.session();
        drop(self.owner.take());
        let mut contained = self.contained.take().expect("finish follows a launch");
        contained.set_label(label(self.harness, session.as_deref()));
        let finished = contained.finish(&keeper).await;
        // Awaited in place, so a cancellation here still finds the writer in
        // `self.writer` for `Drop` to abort.
        let stdin = match self.writer.as_mut() {
            Some(writer) => match writer.await {
                Ok(result) => result,
                Err(e) => Err(anyhow::anyhow!("stdin write task panicked: {e}")),
            },
            None => Ok(()),
        };
        self.writer = None;
        Ended {
            status: finished.status,
            leftovers: finished.leftovers,
            interrupt_requested: requested
                || self.stopped_by_signal
                || finished.interrupt_delivered == Some(true),
            stopped_by_signal: self.stopped_by_signal,
            stdin,
            readers: AbortOnDrop(std::mem::take(&mut self.readers)),
        }
    }
}

impl Drop for Run {
    /// Abandoned before `finish`: the readers and the writer stop, an owned
    /// lease closes (dropping `owner`), and `Contained`'s own `Drop` requests
    /// the teardown and records that nobody saw it finish.
    fn drop(&mut self) {
        for reader in &self.readers {
            reader.abort();
        }
        if let Some(writer) = &self.writer {
            writer.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!("lifecycle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn marker(root: &Path, sid: &str) -> Option<serde_json::Value> {
        crate::inflight::read_live(Some(root))
            .into_iter()
            .find(|m| m.session_id == sid)
            .and_then(|m| serde_json::to_value(&m.marker).ok())
    }

    #[test]
    fn malformed_first_identity_remains_evidence_without_path_authority() {
        let lease = Lease::learned(
            crate::harness::Harness::Codex,
            Path::new("/project"),
            &crate::data::DataDir::unavailable(),
            &crate::sessions::handoff::RunId::default(),
            None,
            None,
        );
        lease.set_session("../malformed");
        lease.set_session("valid");
        assert_eq!(lease.session().as_deref(), Some("../malformed"));
        assert_eq!(
            lease.session_watch().borrow().as_deref(),
            Some("../malformed")
        );
    }

    #[test]
    fn a_session_learned_after_readiness_offers_the_keeper_at_once() {
        let root = scratch("ready-first");
        let owner = Lease::learned(
            Harness::Codex,
            Path::new("/p"),
            &crate::data::DataDir::at(root.clone()),
            &crate::sessions::handoff::RunId::default(),
            None,
            None,
        );
        owner.record_keeper(4242);
        owner.ready();
        assert!(marker(&root, "s1").is_none(), "no id, no marker");
        owner.set_session("s1");
        let m = marker(&root, "s1").expect("marker up");
        assert_eq!(m["keeper"]["pid"], 4242);
        assert_eq!(m["signal_ready"], true);
    }

    #[test]
    fn readiness_after_the_session_offers_the_keeper_then() {
        let root = scratch("session-first");
        let owner = Lease::learned(
            Harness::Codex,
            Path::new("/p"),
            &crate::data::DataDir::at(root.clone()),
            &crate::sessions::handoff::RunId::default(),
            None,
            None,
        );
        owner.record_keeper(7);
        owner.set_session("s2");
        assert_eq!(marker(&root, "s2").expect("marker")["signal_ready"], false);
        owner.ready();
        assert_eq!(marker(&root, "s2").expect("marker")["signal_ready"], true);
    }

    #[test]
    fn a_non_interruptible_harness_never_offers_a_pid() {
        let root = scratch("grok");
        let owner = Lease::known(
            &crate::harness::SessionId::parse("g1").expect("id"),
            Harness::Grok,
            Path::new("/p"),
            &crate::data::DataDir::at(root.clone()),
            &crate::sessions::handoff::RunId::default(),
            None,
            None,
        );
        owner.record_keeper(9);
        owner.ready();
        let m = marker(&root, "g1").expect("marker");
        assert_eq!(m["keeper"]["pid"], 9);
        assert_eq!(m["signal_ready"], false);
    }

    #[test]
    fn aliases_project_existing_readiness_and_reservation_outlives_lease() {
        let root = scratch("alias-ready");
        let data = crate::data::DataDir::at(root.clone());
        let id = crate::sessions::handoff::RunId::default();
        let reserved = crate::inflight::reserve(&data, &id)
            .expect("reserve")
            .map(std::sync::Arc::new);
        let owner = Lease::known(
            &crate::harness::SessionId::parse("primary").expect("id"),
            Harness::Claude,
            Path::new("/p"),
            &data,
            &id,
            reserved.clone(),
            None,
        );
        owner.record_keeper(std::process::id());
        owner.ready();
        owner.alias("alias");
        assert_eq!(marker(&root, "alias").expect("alias")["signal_ready"], true);
        drop(owner);
        assert!(
            crate::inflight::reserve(&data, &id).is_err(),
            "invocation owner still holds namespace"
        );
        drop(reserved);
        assert!(crate::inflight::reserve(&data, &id).is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_first_learned_session_wins() {
        let root = scratch("accept-once");
        let owner = Lease::learned(
            Harness::Codex,
            Path::new("/p"),
            &crate::data::DataDir::at(root.clone()),
            &crate::sessions::handoff::RunId::default(),
            None,
            None,
        );
        owner.set_session("first");
        owner.set_session("second");
        assert_eq!(owner.session().as_deref(), Some("first"));
        assert!(marker(&root, "second").is_none());
    }

    #[test]
    fn settling_consumes_the_request_and_closes_for_good() {
        let root = scratch("settle");
        let owner = Lease::known(
            &crate::harness::SessionId::parse("k1").expect("id"),
            Harness::Claude,
            Path::new("/p"),
            &crate::data::DataDir::at(root.clone()),
            &crate::sessions::handoff::RunId::default(),
            None,
            None,
        );
        crate::inflight::request_interrupt("k1", Some(&root)).expect("request");
        assert!(owner.settle(true), "the request is this run's");
        assert!(marker(&root, "k1").is_none(), "marker removed");
        assert!(!crate::inflight::take_interrupt_request_in(
            "k1",
            &crate::data::DataDir::at(root.clone())
        ));
        // Facts still update after closing; markers never come back.
        let late = Lease::learned(
            Harness::Codex,
            Path::new("/p"),
            &crate::data::DataDir::at(root.clone()),
            &crate::sessions::handoff::RunId::default(),
            None,
            None,
        );
        late.settle(true);
        late.set_session("late");
        assert_eq!(late.session().as_deref(), Some("late"));
        assert!(marker(&root, "late").is_none());
    }

    #[test]
    fn dropping_the_owner_removes_every_marker_and_alias() {
        let root = scratch("owner-drop");
        let owner = Lease::known(
            &crate::harness::SessionId::parse("a").expect("id"),
            Harness::Grok,
            Path::new("/p"),
            &crate::data::DataDir::at(root.clone()),
            &crate::sessions::handoff::RunId::default(),
            None,
            None,
        );
        owner.record_keeper(3);
        owner.alias("b");
        assert_eq!(marker(&root, "b").expect("alias")["keeper"]["pid"], 3);
        let reader_clone = Lease::clone(&owner);
        drop(owner);
        assert!(marker(&root, "a").is_none());
        assert!(marker(&root, "b").is_none());
        reader_clone.alias("c");
        assert!(marker(&root, "c").is_none(), "closed for every holder");
    }

    #[test]
    fn the_spec_layers_env_and_sets_pwd_last() {
        let command = HarnessCommand {
            program: "prog".into(),
            leading_args: vec!["lead".into()],
        };
        let profile = BTreeMap::from([
            ("A".to_string(), "profile".to_string()),
            ("PWD".to_string(), "/elsewhere".to_string()),
            ("F".to_string(), "profile".to_string()),
        ]);
        let spec = ProcessSpec::new(
            &command,
            ["x".to_string()],
            Path::new("/proj"),
            &[("A", "default".into()), ("B", "default".into())],
            Some(&profile),
            &[("F", "forced".into())],
            Stdin::Null,
        );
        assert_eq!(spec.args, ["lead", "x"]);
        assert_eq!(spec.env["A"], "profile", "a profile overrides a default");
        assert_eq!(spec.env["B"], "default");
        assert_eq!(spec.env["F"], "forced", "a forced value beats the profile");
        assert_eq!(spec.env["PWD"], "/proj", "PWD names the working directory");
    }
}
