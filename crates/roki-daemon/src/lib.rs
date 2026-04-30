//! roki-daemon library crate.
//!
//! The binary entry point lives in `src/main.rs` and is named `roki`. The
//! library half exists so integration tests in `tests/` can exercise the CLI
//! parser and the runtime bootstrap helpers without spawning the binary.

pub mod cli;
pub mod config;
pub mod runtime;
