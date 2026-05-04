//! `roki-daemon` library crate.
//!
//! The binary entry point lives in `src/main.rs` (binary name `roki`). The
//! library half exists so unit and integration tests can exercise the parser,
//! configuration, and runtime helpers without spawning the binary.

pub mod cli;
pub mod config;
pub mod logging;
