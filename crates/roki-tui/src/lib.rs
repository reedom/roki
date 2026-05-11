//! Library facade for the roki-tui binary. Integration tests in
//! `crates/roki-tui/tests/` consume `App::run_for_test`.

pub mod app;
pub mod cli;
pub mod client;
pub mod config;
pub mod input;
pub mod model;
pub mod palette;
pub mod poll;
pub mod sanitize;
pub mod startup_log;
pub mod ui;
