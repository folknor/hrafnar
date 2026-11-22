//! `hrafnar` as a library, so the integration tests can drive the harness
//! runners against stub harnesses with the real binary as their keeper (see
//! `keeper`). Cargo hands an integration test the built binary's path
//! (`CARGO_BIN_EXE_hrafnar`) and gives a unit test nothing of the kind, which is
//! the whole reason for the split; nothing here is a public API. `main.rs`
//! has two personalities: `hrafnar __keep <fd>` becomes a keeper before any
//! tokio runtime exists, and anything else goes to `app::run`.
//!
//! # Layers
//!
//! Each module's own `//!` comment says what it owns. Grouped from the command
//! line down to the process a harness runs in:
//!
//! - **Command**: `cli`, `app` (`launch`, `message`, `history`),
//!   `interrupt_cmd`, `config_cmd`, `man`, `exit`
//! - **Config**: `config`, `project`, `config_write`
//! - **Harness**: `harness`, which turns a `LaunchSpec` into a classified
//!   `HarnessResult`, with one adapter per wire protocol (`claude_compat` for
//!   claude and maki, `codex`, `grok`)
//! - **Containment**: `contained`, `keeper`, `supervisor`, `process`, `report`
//! - **State**: `data`, `sessions`, `inflight`, `audit`, `incident`,
//!   `transcript`, `lock`, `fsutil`
//! - **Shared policy**: `writable_roots`, `git_watch`, `watchdog`, `timings`,
//!   `prompt`, `input`, `ids`
//!
//! Dependencies point downward: the command layer picks what to run, the
//! harness layer runs it, containment holds it, and state records it. The
//! harness layer never reads the command line. The keeper and `contained`
//! hold every process the same way, from a `lifecycle::ProcessSpec`, so no
//! harness's behaviour reaches them.
//!
//! # A fresh run
//!
//! `app::run` parses, resolves the project and config, and checks the profile
//! and its chosen harness before anything launches. Under the host launch lock
//! it fixes the audit id, assembles the prompt and spawns `app::launch` with a
//! `LaunchSpec`. `harness::invoke` prepares the adapter, and the adapter's
//! runner drives `lifecycle::Run`: keeper, launch, prompt, stream, wait,
//! teardown verdict. The run task records its audit and session rows as soon
//! as the run ends, and `app::run` prints the result and exits with a
//! `CommandExit`.
//!
//! # Rules that hold across modules
//!
//! - **Every harness process runs under its own keeper**, launched through
//!   `contained::spawn`: runs, auto-resumes, grok wakes, messages and the
//!   incident version probe alike. A run is over only once its keeper has
//!   proved its process tree empty.
//! - **Every runner drives the one lifecycle.** An adapter keeps its own
//!   argv, stream decoding, recovery and classification, and calls
//!   `harness::lifecycle` for the steps whose order matters.
//! - **Harness-specific decisions are exhaustive matches** on
//!   `harness::Harness` inside the harness layer, never a check of a
//!   harness's name elsewhere.
//! - **Config loads strictly** wherever its result decides something (see
//!   `config::load`).
//! - **Plain text everywhere, no JSON output**, for the reason in
//!   `config_cmd`.
//! - **Every run starts a fresh session**: the archetype's prime, if any, then
//!   stdin. Reviving a long-lived session on a cold cache reprocesses its whole
//!   history, while a fresh one costs about one hrafnar's tokens and cannot act
//!   on stale context. Follow-ups go through `hrafnar message` while the cache
//!   is warm.
//!
//! # Tests
//!
//! Unit tests sit beside their code, in `#[cfg(test)]` modules or a sibling
//! `*_tests.rs` or `tests.rs`. The integration tests in `tests/` drive the
//! built binary: `provider_runs.rs` runs each adapter against stub harness
//! scripts with the real keeper, `keeper.rs` speaks the keeper protocol
//! directly, and `man.rs` checks the bundled docs against the code.

pub mod app;
pub mod audit;
pub mod cli;
pub mod config;
pub mod config_cmd;
pub mod config_write;
pub mod contained;
pub mod data;
pub mod exit;
mod fsutil;
pub mod git_watch;
pub mod harness;
pub mod incident;
pub mod inflight;
pub mod input;
pub mod interrupt_cmd;
pub mod keeper;
pub mod lock;
pub mod man;
pub mod project;
pub mod prompt;
pub mod sessions;
pub mod timings;
pub mod transcript;
pub mod watchdog;
pub mod writable_roots;

pub mod process;
pub mod report;
pub mod supervisor;

mod ids;
mod toml_key;

#[cfg(test)]
#[path = "../tests/support/codex.rs"]
mod codex_fixtures;
