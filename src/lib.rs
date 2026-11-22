//! `review` as a library, so the integration tests can drive the harness
//! runners against stub harnesses with the real binary as their keeper (see
//! `keeper`). Cargo hands an integration test the built binary's path
//! (`CARGO_BIN_EXE_review`) and gives a unit test nothing of the kind, which is
//! the whole reason for the split; nothing here is a public API.

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
