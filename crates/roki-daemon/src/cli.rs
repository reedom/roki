//! Command-line surface for the `roki` binary.
//!
//! This is the minimal CLI shell defined by task 1.1. Later tasks extend
//! `RunArgs` with concrete configuration knobs (config path, log level,
//! permission strategy, etc.). The structure here is deliberately additive so
//! those follow-ups do not need to restructure the parser.

use clap::{Parser, Subcommand};

/// Top-level CLI for the roki daemon.
#[derive(Debug, Parser)]
#[command(
    name = "roki",
    version,
    about = "roki daemon: Linear-driven, per-issue agent orchestrator",
    long_about = "roki observes Linear, allocates per-(repo, issue) workspaces, \
                  and supervises long-lived Claude Code subprocesses. The daemon \
                  never writes Linear, never opens PRs, and never edits code; \
                  the agent does all of that inside its sandbox."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands recognized by the daemon.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon (orchestrator, Linear adapter, workflow loader).
    Run(RunArgs),
}

/// Arguments for the `roki run` subcommand.
///
/// Task 1.1 keeps this intentionally empty. Subsequent tasks (1.2 config
/// loader, 1.3 logging, 9.x permissions) add concrete fields here. The struct
/// exists today so the parser can name the subcommand and so future fields
/// land additively without reshaping the CLI.
#[derive(Debug, Default, Parser)]
pub struct RunArgs {}

impl Cli {
    /// Parse arguments from `std::env::args_os`.
    ///
    /// Errors and `--help` output are handled by clap; this wrapper exists so
    /// `main` and integration tests share a single entry point.
    pub fn parse_from_env() -> Self {
        Self::parse()
    }
}
