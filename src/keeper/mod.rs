//! The keeper: one per harness invocation, between `review` and the harness,
//! so that when a run ends nothing it started is left running.
//!
//! # Why a process of its own
//!
//! A harness's descendants do not stay in the process group `review` gives it.
//! codex's Linux sandbox runs bwrap with `--new-session` and unified-exec
//! shells `setsid()`, so a shell the agent started is in a session of its own,
//! and when codex exits it is reparented to init, still able to write the
//! project. Killing the group cannot reach such a process. Making `review`
//! itself the child subreaper is rejected because an adopted orphan carries no
//! mark of the invocation that started it, and because the teardown would die
//! with `review`. So each invocation gets its own subreaper. Everything the
//! harness starts, whatever it does to its session or group, stays a
//! descendant of this process, and this process belongs to exactly one
//! invocation. If `review` dies, the control pipe's EOF tells the keeper to
//! tear down anyway.
//!
//! # The proof of emptiness
//!
//! As a subreaper the keeper is the nearest living ancestor of every process in
//! its tree, so while any of them lives, the keeper has at least one child.
//! `waitpid` returning `ECHILD` therefore proves the tree is empty, and nothing
//! else is accepted as that proof. Discovery (walking
//! `/proc/<pid>/task/<tid>/children`) only finds signal targets: the kernel
//! documents that walk as unreliable while siblings exit, and a leader that
//! called `pthread_exit` has no readable task list, so an empty walk proves
//! nothing and a failed one is never terminal.
//!
//! # Identity
//!
//! Signals go only through pidfds, never to a bare pid. A direct child is
//! admitted by pid (only the keeper can reap it, so its pid cannot be reused
//! under us). A process `c` found under an admitted `p` is admitted when, after
//! reading `c`'s parent from `/proc/c/stat`, both `c`'s and `p`'s pidfds still
//! show them alive: a process alive at the end of the check was alive
//! throughout and held its pid, so the parent read named `p` while `p` owned
//! that number, and `c` is in the tree. Admitting by a parent's pid number
//! alone would admit strangers once that number is reused. pidfd liveness is
//! also right for a zombie leader whose other threads still run, where `/proc`
//! state is not.
//!
//! # Protocol
//!
//! `review` spawns `review __keep <spec-fd>` with SIGINT, SIGTERM, SIGCHLD
//! and SIGPIPE blocked, so no signal can take its default action before the
//! keeper reads them from a signalfd. The spec is a sealed memfd, read and closed
//! before any harness fork; argv carries only its descriptor number, never an
//! environment value. Two pipes beyond stdio, numbered at least 3 so a spawn's
//! stdio setup cannot overwrite them, whose numbers are in the spec: control (review to keeper: `g` go, `i` interrupt, `t`
//! terminate; EOF means `review` is gone) and events (keeper to review, one
//! JSON `Event` per line). The keeper launches nothing before `g`, so `review`
//! can record the keeper's identity first. Its own stdio is the harness's and
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
