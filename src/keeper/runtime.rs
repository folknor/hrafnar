//! Keeper setup, shared state, and the signal/control event loop.

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use super::last_error;
use super::protocol::{Control, Event, Outcome, Proc, Spec};
use crate::process::pidfd_signal;

mod events;
mod launch;
mod teardown;

/// The keeper process. Returns its exit code: 0 once it has run to the end of
/// its protocol, whether or not its events could all be delivered (a reader
/// that stopped draining, or a `hrafnar` that is gone; `hrafnar` judges the
/// report by what reached it, never by this code alone), 1 for an unusable
/// spec or setup, 101 for a panic.
pub fn main() -> i32 {
    // stdio belongs to the harness, so a panic must print nothing; an aborted
    // keeper is reported by `hrafnar` as a keeper failure.
    std::panic::set_hook(Box::new(|_| {}));
    match std::panic::catch_unwind(run_from_args) {
        Ok(Ok(())) => 0,
        Ok(Err(_)) => 1,
        Err(_) => 101,
    }
}

fn run_from_args() -> std::io::Result<()> {
    let fd = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<RawFd>().ok())
        .filter(|fd| *fd >= 3)
        .ok_or_else(|| std::io::Error::other("bad keeper spec fd"))?;
    // SAFETY: validate the inherited descriptor before taking ownership.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(last_error());
    }
    // SAFETY: the private launcher transferred this descriptor to the keeper.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    use std::io::Read;
    let mut payload = Vec::new();
    file.take(super::protocol::SPEC_MAX as u64 + 1)
        .read_to_end(&mut payload)?;
    // take owns and closes the file before any harness can be created.
    if payload.len() > super::protocol::SPEC_MAX {
        return Err(std::io::Error::other("keeper spec too large"));
    }
    let spec: Spec = serde_json::from_slice(&payload).map_err(std::io::Error::other)?;
    if spec.control_fd < 3
        || spec.events_fd < 3
        || spec.control_fd == spec.events_fd
        || spec.control_fd == fd
        || spec.events_fd == fd
    {
        return Err(std::io::Error::other("bad keeper pipe descriptors"));
    }
    for pipe in [spec.control_fd, spec.events_fd] {
        // SAFETY: inspect transferred descriptors before creating any new fd.
        if unsafe { libc::fcntl(pipe, libc::F_GETFD) } < 0 {
            return Err(last_error());
        }
    }
    spec.launch.validate()?;
    spec.schedule.validate()?;
    Keeper::new(spec)?.run()
}

fn check(ret: libc::c_int) -> std::io::Result<libc::c_int> {
    if ret < 0 { Err(last_error()) } else { Ok(ret) }
}

/// The signals the keeper handles through its signalfd.
fn handled_signals() -> libc::sigset_t {
    // SAFETY: sigemptyset/sigaddset only write the set we own.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGCHLD, libc::SIGPIPE] {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

/// Block the handled signals in the calling thread. `hrafnar` calls this in the
/// keeper's pre-exec hook, so they are blocked from the keeper's first
/// instruction; the keeper repeats it, which is harmless.
pub fn block_handled_signals() -> std::io::Result<()> {
    let set = handled_signals();
    // SAFETY: async-signal-safe; only changes the calling thread's mask.
    check(unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) })?;
    Ok(())
}

fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = check(libc::fcntl(fd, libc::F_GETFD))?;
        check(libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = check(libc::fcntl(fd, libc::F_GETFL))?;
        check(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
    }
    Ok(())
}

struct Member {
    pidfd: OwnedFd,
    /// The last command line read while the process was alive. Read again
    /// every round: a process found between its fork and its exec still shows
    /// its parent's command line, and one mid-`execve` shows none.
    proc_: Proc,
    termed: bool,
}

#[derive(PartialEq)]
enum Phase {
    /// Waiting for `g`.
    Waiting,
    /// The harness has been forked; waiting for its exec.
    Starting,
    Running,
    /// Tearing down; never restarted.
    TearingDown,
}

struct Keeper {
    spec: Spec,
    me: u32,
    signals: OwnedFd,
    control: Option<OwnedFd>,
    events: Option<OwnedFd>,
    pending: VecDeque<u8>,
    owner_gone: bool,
    phase: Phase,
    leader: Option<(u32, OwnedFd)>,
    leader_status: Option<i32>,
    exec_status: Option<OwnedFd>,
    interrupted: bool,
    terminate: bool,
    members: BTreeMap<u32, Member>,
    killed: Vec<Proc>,
    killed_dropped: usize,
}

impl Keeper {
    fn new(spec: Spec) -> std::io::Result<Self> {
        block_handled_signals()?;
        let set = handled_signals();
        // SAFETY: creates a descriptor for the blocked set; we own it.
        let sfd =
            check(unsafe { libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) })?;
        // SAFETY: fresh descriptor.
        let signals = unsafe { OwnedFd::from_raw_fd(sfd) };
        // SAFETY: plain prctl on ourselves.
        check(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) })?;
        // Private from here on: the harness inherits neither.
        for fd in [spec.control_fd, spec.events_fd] {
            set_cloexec(fd)?;
            set_nonblocking(fd)?;
        }
        // SAFETY: hrafnar handed us these descriptors, ours alone from here.
        let control = Some(unsafe { OwnedFd::from_raw_fd(spec.control_fd) });
        let events = Some(unsafe { OwnedFd::from_raw_fd(spec.events_fd) });
        Ok(Self {
            spec,
            me: std::process::id(),
            signals,
            control,
            events,
            pending: VecDeque::new(),
            owner_gone: false,
            phase: Phase::Waiting,
            leader: None,
            leader_status: None,
            exec_status: None,
            interrupted: false,
            terminate: false,
            members: BTreeMap::new(),
            killed: Vec::new(),
            killed_dropped: 0,
        })
    }

    fn run(mut self) -> std::io::Result<()> {
        // Nothing launches before `g`: an owner already gone launches nothing.
        while self.phase == Phase::Waiting {
            if self.owner_gone {
                return Ok(());
            }
            if self.terminate {
                // Shutdown reached this keeper before its `g`: say so, with a
                // clean outcome (nothing was started), rather than leave the
                // run's teardown looking unconfirmed.
                self.queue(&Event::ExecFailed {
                    reason: "terminated before the harness started".to_string(),
                });
                self.finish(&Outcome {
                    teardown: super::Teardown::observed(true, Vec::new(), 0, Vec::new()),
                    ..Outcome::default()
                });
                return Ok(());
            }
            self.service(None);
        }
        let started = Instant::now();
        let startup = self.spec.startup;
        while self.phase == Phase::Starting {
            if self.terminate || started.elapsed() >= startup {
                let reason = if self.terminate {
                    "terminated before the harness started".to_string()
                } else {
                    format!("the harness did not start within {}s", startup.as_secs())
                };
                if let Some((_, fd)) = &self.leader {
                    let _ = pidfd_signal(fd, libc::SIGKILL);
                }
                self.queue(&Event::ExecFailed { reason });
                // The killed child closes the exec-status pipe by dying, which
                // would otherwise read as a successful exec mid-teardown.
                self.exec_status = None;
                break;
            }
            self.service(Some(startup.saturating_sub(started.elapsed())));
        }
        while self.phase == Phase::Running {
            if self.terminate || self.leader_status.is_some() {
                break;
            }
            self.service(None);
        }
        let grace = if self.leader_status.is_some() || self.leader.is_none() {
            super::Start::Leftover
        } else {
            super::Start::Terminate
        };
        self.phase = Phase::TearingDown;
        let outcome = self.teardown(grace);
        self.finish(&outcome);
        Ok(())
    }

    /// Wait for at most `timeout` (forever if `None`) for something to happen,
    /// and handle it: signals, control bytes, the harness's exec status, and
    /// draining queued events.
    fn service(&mut self, timeout: Option<Duration>) {
        let mut fds = vec![libc::pollfd {
            fd: self.signals.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let control_at = self.control.as_ref().map(|c| {
            fds.push(libc::pollfd {
                fd: c.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let exec_at = self.exec_status.as_ref().map(|e| {
            fds.push(libc::pollfd {
                fd: e.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let events_at = match (&self.events, self.pending.is_empty()) {
            (Some(ev), false) => {
                fds.push(libc::pollfd {
                    fd: ev.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                });
                Some(fds.len() - 1)
            }
            _ => None,
        };
        let ms = timeout.map_or(-1, |t| i32::try_from(t.as_millis()).unwrap_or(i32::MAX));
        let nfds = libc::nfds_t::try_from(fds.len()).unwrap_or(0);
        // SAFETY: `fds` is a live array of `nfds` pollfds.
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), nfds, ms) };
        if ret <= 0 {
            return;
        }
        if fds[0].revents != 0 {
            self.read_signals();
        }
        if let Some(i) = control_at
            && fds[i].revents != 0
        {
            self.read_control();
        }
        if let Some(i) = exec_at
            && fds[i].revents != 0
        {
            self.read_exec_status();
        }
        if let Some(i) = events_at
            && fds[i].revents != 0
        {
            self.flush();
        }
    }

    fn read_signals(&mut self) {
        let mut info: libc::signalfd_siginfo =
            // SAFETY: plain-old-data, fully overwritten by read.
            unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::signalfd_siginfo>();
        loop {
            // SAFETY: reads at most `size` bytes into `info`.
            let n = unsafe {
                libc::read(
                    self.signals.as_raw_fd(),
                    (&raw mut info).cast::<libc::c_void>(),
                    size,
                )
            };
            if usize::try_from(n).ok() != Some(size) {
                break;
            }
            match libc::c_int::try_from(info.ssi_signo).unwrap_or(0) {
                libc::SIGCHLD => {}
                libc::SIGINT => self.interrupt(),
                libc::SIGTERM => self.terminate = true,
                _ => {}
            }
        }
        self.reap();
    }

    fn read_control(&mut self) {
        let Some(control) = &self.control else {
            return;
        };
        let mut buf = [0u8; 64];
        // SAFETY: reads into a buffer we own.
        let n = unsafe {
            libc::read(
                control.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if n == 0 {
            self.owner_gone();
            return;
        }
        let Ok(n) = usize::try_from(n) else {
            if last_error().kind() != std::io::ErrorKind::WouldBlock {
                self.owner_gone();
            }
            return;
        };
        for &b in &buf[..n] {
            match Control::from_byte(b) {
                // Never once termination has begun: a `t` (or SIGTERM) ahead
                // of the `g` means shutdown closed admission first.
                Some(Control::Go) if self.phase == Phase::Waiting && !self.terminate => {
                    self.launch();
                }
                Some(Control::Interrupt) => self.interrupt(),
                Some(Control::Terminate) => self.terminate = true,
                _ => {}
            }
        }
    }

    /// `hrafnar` is gone: tear down, and keep the outcome for the orphan log.
    fn owner_gone(&mut self) {
        self.owner_gone = true;
        self.control = None;
        self.terminate = true;
    }

    /// Forward the first interrupt to the harness, once, while it runs.
    fn interrupt(&mut self) {
        // `interrupted` records successful delivery and suppresses repeats.
        // Caller intent remains independent. Supported callers wait for harness output
        // before sending `i` or SIGINT, and terminate before readiness so a
        // pre-exec child is not signalled with its default handler.
        if self.phase != Phase::Running || self.interrupted || self.leader_status.is_some() {
            return;
        }
        if let Some((_, fd)) = &self.leader {
            self.interrupted = pidfd_signal(fd, libc::SIGINT).is_ok();
        }
    }

    /// Reap every child that has exited, recording the harness's status.
    /// Returns true on `ECHILD`: no children at all.
    fn reap(&mut self) -> bool {
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: plain waitpid on our own children.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid > 0 {
                let pid = u32::try_from(pid).unwrap_or(0);
                if self.leader.as_ref().is_some_and(|(l, _)| *l == pid) {
                    self.leader_status = Some(status);
                    self.queue(&Event::Exited { status });
                }
                continue;
            }
            if pid == 0 {
                return false;
            }
            return last_error().raw_os_error() == Some(libc::ECHILD);
        }
    }
}
