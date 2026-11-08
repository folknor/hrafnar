//! Shared Linux process identity and pidfd primitives. Membership of a
//! keeper tree and ownership of an interrupt target remain caller policies.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// How a reaped process ended, decoded once from its raw wait status. The
/// keeper's wire keeps the raw `i32` (a keeper can be a different build than
/// the `review` reading it, when the binary is replaced mid-run, and the raw
/// status is the most stable encoding); this is what `review` reasons over.
/// Forensics only: no runner classifies a turn by it, since opencode exits 130
/// on a clean SIGINT and maki 0 on a failed turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    /// Never ran, could not be reaped, or the keeper's report did not arrive.
    #[default]
    Unknown,
}

impl Exit {
    pub fn from_wait_status(raw: i32) -> Self {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(raw);
        match (status.code(), status.signal()) {
            (Some(code), _) => Self::Code(code),
            (None, Some(sig)) => Self::Signal(sig),
            (None, None) => Self::Unknown,
        }
    }

    pub fn code(self) -> Option<i32> {
        match self {
            Self::Code(code) => Some(code),
            _ => None,
        }
    }

    pub fn success(self) -> bool {
        self == Self::Code(0)
    }

    pub fn is_signal(self) -> bool {
        matches!(self, Self::Signal(_))
    }

    /// The signal's name as the logs have always spelled it (`SIGINT`,
    /// `SIGKILL`, `SIGTERM`, else `signal N`).
    pub fn signal_name(self) -> Option<String> {
        match self {
            Self::Signal(sig) => Some(match sig {
                libc::SIGINT => "SIGINT".to_string(),
                libc::SIGKILL => "SIGKILL".to_string(),
                libc::SIGTERM => "SIGTERM".to_string(),
                other => format!("signal {other}"),
            }),
            _ => None,
        }
    }
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

pub fn pidfd_open(pid: u32) -> std::io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).map_err(std::io::Error::other)?;
    // SAFETY: plain syscall; the returned descriptor is ours to own.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(last_error());
    }
    let fd = RawFd::try_from(fd).map_err(std::io::Error::other)?;
    // SAFETY: a fresh descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Whether the process behind `pidfd` has exited (its pidfd is readable). For
/// a multithreaded process that is when its last thread has exited, unlike its
/// leader's `/proc` state.
pub fn pidfd_exited(pidfd: &OwnedFd) -> bool {
    let mut pfd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd we own, no timeout.
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    ret > 0 && pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

pub fn pidfd_signal(pidfd: &OwnedFd, sig: libc::c_int) -> std::io::Result<()> {
    // SAFETY: plain syscall on a pidfd we own; no siginfo.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if ret < 0 { Err(last_error()) } else { Ok(()) }
}

/// `pid`'s parent and start time, from `/proc/<pid>/stat`. `comm` may hold
/// spaces and parentheses, so fields are read after the last `)`: state is
/// field 3, ppid 4, starttime 22.
pub fn stat_ppid_start(pid: u32) -> Option<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((ppid, start))
}

/// Whether the process recorded as `pid` with start time `start` is alive.
/// Through a pidfd: opened first, the start time read after, then the pidfd
/// checked still unexited, so the start time read was this process's. A zombie
/// leader whose other threads still run counts as alive, which its `/proc`
/// state would not.
pub fn identity_alive(pid: u32, start: Option<u64>) -> bool {
    let Some(start) = start else {
        return pid_alive(pid);
    };
    let Ok(pidfd) = pidfd_open(pid) else {
        return false;
    };
    stat_ppid_start(pid).is_some_and(|(_, s)| s == start) && !pidfd_exited(&pidfd)
}

/// Is `pid` a live process? Signal 0 performs permission and existence checks
/// without delivering anything. `ESRCH` means gone; `EPERM` means it exists but
/// belongs to someone else, which still counts as alive.
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` with signal 0 delivers nothing; it only reports whether the
    // pid exists. No memory is touched.
    let ret = unsafe { libc::kill(pid, 0) };
    ret == 0 || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_knows_its_parent_and_start_time() {
        // SAFETY: getppid has no failure mode.
        let parent = u32::try_from(unsafe { libc::getppid() }).expect("pid fits");
        let (ppid, start) = stat_ppid_start(std::process::id()).expect("own stat");
        assert_eq!(ppid, parent);
        assert!(start > 0);
    }

    #[test]
    fn recorded_identity_checks_start_time_and_legacy_existence() {
        let pid = std::process::id();
        let (_, start) = stat_ppid_start(pid).expect("own stat");
        assert!(identity_alive(pid, Some(start)));
        assert!(!identity_alive(pid, Some(start + 1)));
        assert!(identity_alive(pid, None));
        assert!(!identity_alive(u32::MAX, None));
    }

    #[test]
    fn wait_statuses_decode_into_one_exit() {
        assert_eq!(Exit::from_wait_status(7 << 8), Exit::Code(7));
        assert_eq!(
            Exit::from_wait_status(libc::SIGTERM),
            Exit::Signal(libc::SIGTERM)
        );
        assert_eq!(
            Exit::Signal(libc::SIGKILL).signal_name().as_deref(),
            Some("SIGKILL")
        );
        assert_eq!(Exit::Signal(6).signal_name().as_deref(), Some("signal 6"));
        assert!(Exit::Code(0).success() && !Exit::Unknown.success());
    }

    #[test]
    fn a_live_process_has_an_unexited_pidfd() {
        let fd = pidfd_open(std::process::id()).expect("own pidfd");
        assert!(!pidfd_exited(&fd));
    }
}
