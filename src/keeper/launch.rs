//! Provider fork/exec and the exec-status handshake.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use super::super::process::pidfd_open;
use super::super::protocol::Event;
use super::{Keeper, Phase, check, last_error};

impl Keeper {
    pub(super) fn launch(&mut self) {
        match self.fork_provider() {
            Ok(()) => self.phase = Phase::Starting,
            Err(reason) => {
                self.queue(&Event::ExecFailed { reason });
                self.phase = Phase::TearingDown;
            }
        }
    }

    fn fork_provider(&mut self) -> Result<(), String> {
        let program = &self.spec.program;
        let path = if program.contains('/') {
            PathBuf::from(program)
        } else {
            which::which(program).map_err(|e| format!("{program}: {e}"))?
        };
        let to_c = |s: &str| std::ffi::CString::new(s).map_err(|e| e.to_string());
        let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| e.to_string())?;
        let mut argv_c = vec![to_c(program)?];
        for a in &self.spec.args {
            argv_c.push(to_c(a)?);
        }
        let mut argv: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());
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
                // The Rust runtime ignores SIGPIPE; a provider gets the
                // default, as it would from `std::process::Command`.
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::execv(path_c.as_ptr(), argv.as_ptr());
                let errno = *libc::__errno_location();
                let bytes = errno.to_ne_bytes();
                libc::write(
                    status_w.as_raw_fd(),
                    bytes.as_ptr().cast::<libc::c_void>(),
                    bytes.len(),
                );
                libc::_exit(127);
            }
        }
        drop(status_w);
        // The provider has its stdio; the keeper's copies would only hold its
        // pipes open past its exit, delaying `review`'s EOF and its EPIPE on a
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
            // Closed by a successful exec.
            if let Some((pid, _)) = &self.leader {
                let pid = *pid;
                self.queue(&Event::Spawned { pid });
            }
            self.phase = Phase::Running;
        } else if n == 4 {
            let err = std::io::Error::from_raw_os_error(i32::from_ne_bytes(buf));
            self.queue(&Event::ExecFailed {
                reason: format!("{}: {err}", self.spec.program),
            });
            self.phase = Phase::TearingDown;
        } else {
            // A short or failed read: not proof of an exec either way.
            self.queue(&Event::ExecFailed {
                reason: "could not read the provider's exec status".to_string(),
            });
            self.phase = Phase::TearingDown;
        }
    }
}
