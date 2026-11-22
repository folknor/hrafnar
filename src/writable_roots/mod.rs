//! Writable roots a build needs that live outside the workspace.
//!
//! `codex --sandbox workspace-write` grants exactly two things: the cwd and
//! `/tmp`. That is not enough to run a build on any of the hosts this tool is
//! used from, and the shortfall is not the project's to know about: it is a
//! property of the machine.
//!
//! - The build wrapper takes its lock in `$HOME/.brokkr`. Failing that write is
//!   **fatal**: the run dies at `lock: failed to open lock file` before a
//!   single crate is compiled. The same directory holds its `--commit`
//!   worktrees (`$HOME/.brokkr/worktrees/`), which an A/B against another
//!   commit has to create and build in.
//! - It records every invocation in `$XDG_DATA_HOME/brokkr/history.db`.
//!   Failing that write is silent, so sandboxed runs drop out of the history.
//! - Some hosts export a global `CARGO_TARGET_DIR` onto a separate drive, and
//!   some repos carry a `target` symlink pointing at a shared cache. Cargo
//!   cannot write there either.
//! - Cargo downloads crates into `$CARGO_HOME` (`$HOME/.cargo` by default).
//!   This one fails *conditionally*: a workspace whose dependencies are
//!   already unpacked builds, lints and tests fine without it, because cargo's
//!   package-cache lock degrades silently on a read-only filesystem. The first
//!   build that has to fetch a crate dies before compiling, naming the
//!   registry cache rather than the sandbox.
//!
//! Requiring each `.review.toml` to restate those paths would put a
//! machine-shaped fact in every project file, and get it wrong on the hosts
//! that were not in front of whoever edited them. So `review` derives them.
//!
//! Roots are derived against the project root, not the process cwd: the
//! harness runs with the project root as its cwd, and `.review.toml`
//! discovery allows launching from a descendant, whose `target` is not the
//! build's.
//!
//! This is deliberately the *one* place where ambient environment is allowed to
//! influence a run, and it is narrowed hard to stay defensible:
//!
//! - derivation applies to Codex workspace-write and fresh Grok workspace-write
//!   launches. Restored generated Grok profiles retain their recorded canonical
//!   root set instead. Other levels and harnesses derive nothing;
//! - it derives only paths that resolve **identically from every login path**.
//!   `$CARGO_TARGET_DIR` and the `./target` symlink are cargo facts. The build
//!   lock is not a fact about Rust at all (it is derived because brokkr locks
//!   there), but `$HOME/.brokkr` is a fixed, documented location, so deriving
//!   it is reading a stable contract rather than predicting a behaviour. The
//!   test is determinism, not whether a variable happens to look standard:
//!   `$XDG_RUNTIME_DIR` looks generic and is fragile, because a login that
//!   skips pam_systemd never sets it;
//! - it grants only paths that a build provably needs, and only ones that
//!   neither sit **inside** the workspace (writable anyway, so granting them
//!   widens nothing while implying it did) nor **contain** it (`~` and a
//!   `$HOME` that lost its subdirectory both resolve to the home directory,
//!   which is never the narrow grant this promises), and never one carrying
//!   `..`, which would defeat both checks lexically and let codex resolve `/..`
//!   to `/`;
//! - every grant carries a reason and is printed at launch, so the effective
//!   permissions of a run are visible rather than inferred;
//! - it is passed *before* profile `config` overrides, so a profile can still
//!   restate `sandbox_workspace_write.writable_roots` and win.

mod dependencies;
mod derivation;
mod filter;
mod host;
mod profile;

#[cfg(test)]
mod test_host;

pub use derivation::derive;
pub use host::{Host, RealHost};
pub use profile::derive_with;

/// One derived grant: the path, and why a build needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedRoot {
    pub path: String,
    pub why: &'static str,
}
