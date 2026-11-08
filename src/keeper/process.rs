//! Keeper tree discovery and bounded command-line collection.
//! Shared process identity primitives live in `crate::process`.

use std::time::Instant;

use super::protocol::Proc;
use crate::process::stat_ppid_start;

/// Longest command line kept for a process, in bytes.
const CMDLINE_MAX: usize = 512;

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
