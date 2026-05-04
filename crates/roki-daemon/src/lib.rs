//! `roki-daemon` library crate.
//!
//! The binary entry point lives in `src/main.rs` (binary name `roki`). The
//! library half exists so unit and integration tests can exercise the parser,
//! configuration, and runtime helpers without spawning the binary.

pub mod cli;
pub mod config;
pub mod engine;
pub mod exec;
pub mod logging;
pub mod orchestrator;
pub mod permissions;
pub mod runtime;
pub mod session;
pub mod shutdown;
pub mod tracker;
pub mod workflow;
pub mod worktree_manager;
