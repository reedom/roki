//! roki-daemon library crate.
//!
//! The binary entry point lives in `src/main.rs` and is named `roki`. The
//! library half exists so integration tests in `tests/` can exercise the CLI
//! parser and the runtime bootstrap helpers without spawning the binary.

pub mod cli;
pub mod config;
pub mod engine;
pub mod logging;
pub mod orchestrator;
pub mod permissions;
pub mod routing;
pub mod runtime;
pub mod shutdown;
pub mod tools;
pub mod tracker;
pub mod workflow;
pub mod workspace;
