//! Control bytes, launch specifications, and keeper reports.

use serde::{Deserialize, Serialize};
use std::os::fd::RawFd;
use std::path::PathBuf;

/// The hidden first argument that turns a `hrafnar` process into a keeper.
pub(super) const SPEC_MAX: usize = 16 << 20;

pub const ARG: &str = "__keep";

/// How many cleanly killed processes an outcome names. Survivors are never
/// capped: the ticket's requirement is that every one is named.
pub const KILLED_MAX: usize = 64;

/// A byte in the hrafnar-to-keeper control pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Control {
    Go = b'g',
    Interrupt = b'i',
    Terminate = b't',
}

impl Control {
    pub const fn byte(self) -> u8 {
        self as u8
    }

    pub const fn from_byte(byte: u8) -> Option<Self> {
        if byte == Self::Go as u8 {
            Some(Self::Go)
        } else if byte == Self::Interrupt as u8 {
            Some(Self::Interrupt)
        } else if byte == Self::Terminate as u8 {
            Some(Self::Terminate)
        } else {
            None
        }
    }
}

/// What `hrafnar` asks the keeper to run, and how patiently to tear it down.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Spec {
    pub launch: super::Launch,
    pub control_fd: RawFd,
    pub events_fd: RawFd,
    pub schedule: super::TeardownSchedule,
    pub startup: std::time::Duration,
    /// Appended to (one JSON line) when `hrafnar` is gone and the outcome has
    /// nobody to go to.
    pub orphan_log: Option<PathBuf>,
    /// What this run is, for that log line.
    pub label: String,
}

/// A process the teardown dealt with.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub cmdline: String,
    /// The command line was cut at `CMDLINE_MAX`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// What the teardown established.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Outcome<Processes = Vec<Proc>> {
    /// The harness's raw wait status; `None` when it never ran or could not
    /// be reaped.
    pub status: Option<i32>,
    pub teardown: super::Teardown<Processes>,
    /// A successful pidfd_send_signal(SIGINT), not proof it was handled.
    pub interrupt_delivered: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The harness's exec-status pipe closed with no errno: a launch
    /// acknowledgement, not proof that it exec'd. A child killed by someone
    /// else before its exec reports this too, and only `Exited` carries the
    /// truth. The two can arrive in either order, since the keeper may reap
    /// a dead child before it reads the pipe.
    Spawned {
        pid: u32,
    },
    ExecFailed {
        reason: String,
    },
    /// The harness itself has exited and been reaped; the teardown of what it
    /// left follows. Lets `hrafnar` stop watching the harness (the watchdog,
    /// interrupts) without waiting out the teardown.
    Exited {
        status: i32,
    },
    Survivor(Proc),
    Killed(Proc),
    /// Compact counts complete the bounded process-event streams.
    Done(Outcome<usize>),
}
