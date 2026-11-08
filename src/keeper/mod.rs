//! The keeper: one per provider invocation, between `review` and the provider,
//! so that when a run ends nothing it started is left running.
//!
//! # Why a process of its own
//!
//! A provider's descendants do not stay in the process group `review` gives it.
//! codex's Linux sandbox runs bwrap with `--new-session` and unified-exec
//! shells `setsid()`, so a shell the agent started is in a session of its own,
//! and when codex exits it is reparented to init. That is how two hung codemods
//! from a finished run kept a machine busy for five hours while `review` had
//! reported the run over. Killing the group cannot reach such a process, and
//! making `review` itself a child subreaper cannot attribute one: with several
//! runs in one `review`, an adopted orphan may belong to a run that is still
//! going, and killing it destroys live work. So each invocation gets its own
//! subreaper. Everything the provider starts, whatever it does to its session
//! or group, stays a descendant of this process, and this process belongs to
//! exactly one run.
//!
//! # The proof of emptiness
//!
//! As a subreaper the keeper is the nearest living ancestor of every process in
//! its tree, so while any of them lives, the keeper has at least one child.
//! `waitpid` returning `ECHILD` therefore proves the tree is empty, and nothing
//! else is accepted as that proof. Discovery (walking
//! `/proc/<pid>/task/<tid>/children`) only finds signal targets: the kernel
//! documents that walk as unreliable while siblings exit, so an empty walk
//! proves nothing.
//!
//! # Identity
//!
//! Signals go only through pidfds, never to a bare pid. A direct child is
//! admitted by pid (only the keeper can reap it, so its pid cannot be reused
//! under us). A process `c` found under an admitted `p` is admitted when, after
//! reading `c`'s parent from `/proc/c/stat`, both `c`'s and `p`'s pidfds still
//! show them alive: a process alive at the end of the check was alive
//! throughout and held its pid, so the parent read named `p` while `p` owned
//! that number, and `c` is in the tree.
//!
//! # Protocol
//!
//! `review` spawns `review __keep <spec-fd>` with SIGINT, SIGTERM, SIGCHLD
//! and SIGPIPE blocked, so no signal can take its default action before the
//! keeper reads them from a signalfd. The spec is a sealed memfd, read and closed
//! before any provider fork; argv carries only its descriptor number. Two pipes beyond stdio, whose numbers are
//! in the spec: control (review to keeper: `g` go, `i` interrupt, `t`
//! terminate; EOF means `review` is gone) and events (keeper to review, one
//! JSON `Event` per line). The keeper launches nothing before `g`, so `review`
//! can record the keeper's identity first. Its own stdio is the provider's and
//! it never writes to it.

mod launch;
mod policy;
mod process;
mod protocol;
mod runtime;

pub use launch::{Environment, Launch, Launcher};
pub use policy::{Start, Teardown, TeardownSchedule};
pub use protocol::{ARG, Control, Event, KILLED_MAX, Outcome, Proc, Spec};
pub use runtime::{block_handled_signals, main};

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}
