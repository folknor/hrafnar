//! Harness fork/exec and the exec-status handshake.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use super::super::protocol::Event;
use super::{Keeper, Phase, check, last_error};
use crate::process::pidfd_open;

impl Keeper {
    pub(super) fn launch(&mut self) {
        match self.fork_harness() {
            Ok(()) => self.phase = Phase::Starting,
            Err(reason) => {
                self.queue(&Event::ExecFailed { reason });
                self.phase = Phase::TearingDown;
            }
        }
    }

    fn fork_harness(&mut self) -> Result<(), String> {
        self.spec.launch.validate().map_err(|e| e.to_string())?;
        let launch = &self.spec.launch;
        let program = std::ffi::OsStr::from_bytes(&launch.program);
        let cwd = PathBuf::from(std::ffi::OsStr::from_bytes(&launch.cwd));
        let path = if launch.program.contains(&b'/') {
            if PathBuf::from(program).is_absolute() {
                PathBuf::from(program)
            } else {
                cwd.join(program)
            }
        } else {
            let path = launch
                .env
                .iter()
                .find(|(k, _)| k == b"PATH")
                .map_or(std::ffi::OsStr::new("/bin:/usr/bin"), |(_, v)| {
                    std::ffi::OsStr::from_bytes(v)
                });
            // which expands a leading ~ using the keeper's home. Exec PATH
            // has no tilde expansion: make even empty and ~ components absolute
            // under the child cwd before asking its executable checker.
            let paths =
                std::env::join_paths(std::env::split_paths(path).map(|part| cwd.join(part)))
                    .map_err(|e| e.to_string())?;
            which::which_in(program, Some(paths), &cwd)
                .map_err(|e| format!("{}: {e}", program.to_string_lossy()))?
        };
        let to_c = |s: &[u8]| std::ffi::CString::new(s).map_err(|e| e.to_string());
        let path_c = to_c(path.as_os_str().as_bytes())?;
        let cwd_c = to_c(&launch.cwd)?;
        let mut argv_c = vec![to_c(&launch.program)?];
        for a in &launch.args {
            argv_c.push(to_c(a)?);
        }
        let env_c: Vec<_> = launch
            .env
            .iter()
            .map(|(k, v)| {
                let mut entry = k.clone();
                entry.push(b'=');
                entry.extend(v);
                to_c(&entry)
            })
            .collect::<Result<_, _>>()?;
        let mut env: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
        env.push(std::ptr::null());
        let mut argv: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());
        let hold = launch.hold_before_exec;
        let mut status_fds = [0 as RawFd; 2];
        // SAFETY: fills two descriptors we then own.
        check(unsafe { libc::pipe2(status_fds.as_mut_ptr(), libc::O_CLOEXEC) })
            .map_err(|e| format!("exec status pipe: {e}"))?;
        // SAFETY: fresh descriptors.
        let (status_r, status_w) = unsafe {
            (
                OwnedFd::from_raw_fd(status_fds[0]),
                OwnedFd::from_raw_fd(status_fds[1]),
            )
        };
        // SAFETY: this process is single-threaded, so the child may run
        // anything; it still does only what is needed before exec.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!("fork: {}", last_error()));
        }
        if pid == 0 {
            // SAFETY: the child, about to exec or exit.
            unsafe {
                libc::setpgid(0, 0);
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                // The Rust runtime ignores SIGPIPE; a harness gets the
                // default, as it would from `std::process::Command`.
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                if hold {
                    // Test seam (`Launch::hold_before_exec`): announce, then
                    // wait to be killed with `status_w` still open, so its
                    // close is the death and not an exec or an errno write.
                    let line = format!("{}\n", libc::getpid());
                    libc::write(1, line.as_ptr().cast::<libc::c_void>(), line.len());
                    loop {
                        libc::pause();
                    }
                }
                if libc::chdir(cwd_c.as_ptr()) == 0 {
                    libc::execve(path_c.as_ptr(), argv.as_ptr(), env.as_ptr());
                }
                let errno = *libc::__errno_location();
                let bytes = errno.to_ne_bytes();
                loop {
                    let n = libc::write(
                        status_w.as_raw_fd(),
                        bytes.as_ptr().cast::<libc::c_void>(),
                        bytes.len(),
                    );
                    if n >= 0 || *libc::__errno_location() != libc::EINTR {
                        break;
                    }
                }
                libc::_exit(127);
            }
        }
        drop(status_w);
        // The harness has its stdio; the keeper's copies would only hold its
        // pipes open past its exit, delaying `hrafnar`'s EOF and its EPIPE on a
        // prompt nobody reads.
        if let Ok(null) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            for fd in 0..3 {
                // SAFETY: replaces our own stdio with /dev/null.
                unsafe { libc::dup2(null.as_raw_fd(), fd) };
            }
        }
        let raw_pid = pid;
        let pid = u32::try_from(pid).map_err(|e| e.to_string())?;
        let pidfd = match pidfd_open(pid) {
            Ok(fd) => fd,
            Err(e) => {
                // Without a pidfd the teardown could never signal it, so it
                // must not run unsupervised. Our own unreaped child, so its
                // pid cannot have been reused: killing it by pid is safe.
                // SAFETY: plain kill of our own child; reaped by `reap`.
                unsafe { libc::kill(raw_pid, libc::SIGKILL) };
                return Err(format!("pidfd_open: {e}"));
            }
        };
        self.leader = Some((pid, pidfd));
        self.exec_status = Some(status_r);
        Ok(())
    }

    pub(super) fn read_exec_status(&mut self) {
        let Some(fd) = self.exec_status.take() else {
            return;
        };
        let mut buf = [0u8; 4];
        // SAFETY: reads into a buffer we own.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast::<libc::c_void>(), 4) };
        if n == 0 {
            // EOF is an acknowledgement, not proof of exec. A successful exec
            // closes the pipe, and so does a child killed by someone else
            // before its exec (the setup calls before it are unchecked, and
            // scheduling puts no bound on that window). Nothing can tell the
            // two apart once the child is dead: an exec followed by an
            // immediate death looks the same, so classifying on "already
            // dead when inspected" would judge identical runs by timing.
            // Nothing downstream needs to: `Spawned` only resolves the
            // launch to a pid, readiness comes from the harness's own
            // stream, and the true wait status still follows in `Exited` and
            // `Done`. `tests/keeper.rs` holds a child before exec and kills it
            // to keep that so.
            if let Some((pid, _)) = &self.leader {
                let pid = *pid;
                self.queue(&Event::Spawned { pid });
            }
            self.phase = Phase::Running;
        } else if n == 4 {
            let err = std::io::Error::from_raw_os_error(i32::from_ne_bytes(buf));
            self.queue(&Event::ExecFailed {
                reason: format!(
                    "{}: {err}",
                    String::from_utf8_lossy(&self.spec.launch.program)
                ),
            });
            self.phase = Phase::TearingDown;
        } else {
            // A short or failed read: not proof of an exec either way.
            self.queue(&Event::ExecFailed {
                reason: "could not read the harness's exec status".to_string(),
            });
            self.phase = Phase::TearingDown;
        }
    }
}
