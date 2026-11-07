//! Control bytes, launch specifications, and keeper reports.

use serde::{Deserialize, Serialize};
use std::os::fd::RawFd;
use std::path::PathBuf;

/// The hidden first argument that turns a `review` process into a keeper.
pub const ARG: &str = "__keep";

/// How many cleanly killed processes an outcome names. Survivors are never
/// capped: the ticket's requirement is that every one is named.
pub const KILLED_MAX: usize = 64;

/// A byte in the review-to-keeper control pipe.
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

/// What `review` asks the keeper to run, and how patiently to tear it down.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Spec {
    pub program: String,
    pub args: Vec<String>,
    pub control_fd: RawFd,
    pub events_fd: RawFd,
    /// SIGTERM grace for what a provider left behind after exiting on its own.
    pub leftover_grace_ms: u64,
    /// SIGTERM grace when told to terminate a provider that is still running.
    pub terminate_grace_ms: u64,
    /// After the SIGTERM grace: how long SIGKILL rounds may take, leaves first.
    pub kill_phase_ms: u64,
    /// Pause between teardown rounds, which lets a supervising ancestor reap a
    /// killed child and run its own cleanup.
    pub beat_ms: u64,
    /// How long the provider may take to exec before the launch is failed.
    pub startup_ms: u64,
    /// How long the final events may take to drain before the keeper exits.
    pub flush_ms: u64,
    /// Appended to (one JSON line) when `review` is gone and the outcome has
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
pub struct Outcome {
    /// The provider's raw wait status; `None` when it never ran or could not
    /// be reaped.
    pub status: Option<i32>,
    /// `ECHILD`: nothing the provider started is alive.
    pub clean: bool,
    /// Processes left behind that the teardown ended, up to `KILLED_MAX`.
    pub killed: Vec<Proc>,
    /// Further killed processes not named in `killed`.
    #[serde(default)]
    pub killed_dropped: usize,
    /// Processes still alive that the keeper could name; each was sent as a
    /// `Survivor` event before this outcome.
    pub survivors: usize,
    /// Processes remain that discovery could not name, so `survivors` is a
    /// lower bound.
    pub incomplete: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Spawned {
        pid: u32,
    },
    ExecFailed {
        reason: String,
    },
    /// The provider itself has exited and been reaped; the teardown of what it
    /// left follows. Lets `review` stop watching the provider (the watchdog,
    /// interrupts) without waiting out the teardown.
    Exited {
        status: i32,
    },
    Survivor(Proc),
    Done(Outcome),
}
