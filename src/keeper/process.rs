//! Linux process identity, command lines, and child discovery.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Instant;

use super::last_error;
use super::protocol::Proc;

/// Longest command line kept for a process, in bytes.
const CMDLINE_MAX: usize = 512;

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

/// `pid`'s command line, and whether it is only the `comm` fallback (an empty
/// command line: mid-`execve`, a zombie, or a process that cleared its argv).
pub(super) fn read_cmdline(pid: u32) -> (Proc, bool) {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let truncated = raw.len() > CMDLINE_MAX;
    let mut text: String = String::from_utf8_lossy(&raw[..raw.len().min(CMDLINE_MAX)])
        .chars()
        .map(|c| if c == '\0' || c.is_control() { ' ' } else { c })
        .collect();
    text.truncate(text.trim_end().len());
    let fallback = text.is_empty();
    if fallback {
        // Its comm still says what it is.
        text = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|c| format!("[{}]", c.trim()))
            .unwrap_or_else(|_| "?".to_string());
    }
    (
        Proc {
            pid,
            cmdline: text,
            truncated,
        },
        fallback,
    )
}

fn past(deadline: Instant) -> std::io::Result<()> {
    if Instant::now() >= deadline {
        Err(std::io::Error::other("teardown deadline"))
    } else {
        Ok(())
    }
}

/// Children of `pid` across all its threads. `Err` when they cannot be read,
/// or `deadline` passes first, which leaves discovery incomplete rather than
/// empty.
pub(super) fn children_of(pid: u32, deadline: Instant) -> std::io::Result<Vec<u32>> {
    let mut out = Vec::new();
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task"))?;
    let mut any = false;
    for task in tasks {
        past(deadline)?;
        let task = task?;
        let content = match std::fs::read_to_string(task.path().join("children")) {
            Ok(c) => c,
            // Without CONFIG_PROC_CHILDREN the file does not exist; scan /proc
            // for this parent instead.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return children_by_scan(pid, deadline);
            }
            Err(e) => return Err(e),
        };
        any = true;
        out.extend(
            content
                .split_whitespace()
                .filter_map(|p| p.parse::<u32>().ok()),
        );
    }
    if !any {
        // The thread list is gone (the leader exited while other threads
        // live, or the process is a zombie): its children cannot be named.
        return Err(std::io::Error::other("no readable tasks"));
    }
    Ok(out)
}

fn children_by_scan(parent: u32, deadline: Instant) -> std::io::Result<Vec<u32>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc")?.flatten() {
        past(deadline)?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if stat_ppid_start(pid).is_some_and(|(ppid, _)| ppid == parent) {
            out.push(pid);
        }
    }
    Ok(out)
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
    fn a_live_process_has_an_unexited_pidfd() {
        let fd = pidfd_open(std::process::id()).expect("own pidfd");
        assert!(!pidfd_exited(&fd));
    }
}
