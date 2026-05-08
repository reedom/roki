//! CLI parser for the roki binary.
//!
//! Top-level `roki` command exposes one subcommand (`run`) and one flag
//! (`--config <path>` on `run`). [`run`] parses argv, dispatches the
//! matched subcommand, and returns an [`ExitCode`] propagated by `main`.
//!
//! The `--config` doc comment intentionally names `roki.toml` so that
//! `clap`'s rendered `--help` lists the configuration file the flag
//! identifies (Req 1.3).
//!
//! Walking-skeleton task 5.2 wires this module; task 5.3 will call
//! [`run`] from `main`. Until then the module is exercised only by the
//! unit tests below, so suppress `dead_code` for the public surface in
//! line with the convention used by `runtime`, `admission`, etc.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::runtime;

/// roki — Linear-driven coding-agent daemon.
#[derive(Debug, Parser)]
#[command(name = "roki", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

/// Typed subcommand surface. Today the binary exposes two subcommands
/// (`run` and `cleanup`); future tasks add admin / diagnostic verbs as
/// additional variants.
#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Start the daemon with default dispatch (cleanup-first then rule).
    Run {
        /// Path to the roki.toml configuration file.
        #[arg(long = "config", value_name = "PATH")]
        config: PathBuf,
    },
    /// Cleanup-only dispatch: only [[cleanup]] matches lead to a cycle.
    /// [[rule]] list is ignored. Same single-shot binary lifecycle as `run`.
    Cleanup {
        /// Path to the roki.toml configuration file.
        #[arg(long = "config", value_name = "PATH")]
        config: PathBuf,
    },
}

/// Parse argv from the process and dispatch the matched subcommand.
///
/// `clap::Parser::parse` exits the process with a non-zero status on a
/// parse error (e.g. missing `--config`), so the caller never observes
/// that failure path here. Successful parses are forwarded to the
/// matching runtime entry point.
pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Run { config } => runtime::run(&config, runtime::DispatchMode::Default).await,
        CliCommand::Cleanup { config } => {
            runtime::run(&config, runtime::DispatchMode::CleanupOnly).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn run_subcommand_requires_config_flag() {
        let res = Cli::try_parse_from(["roki", "run"]);
        assert!(res.is_err(), "missing --config should error");
    }

    #[test]
    fn run_with_config_flag_parses() {
        let cli = Cli::try_parse_from(["roki", "run", "--config", "/tmp/roki.toml"])
            .expect("should parse");
        match cli.command {
            CliCommand::Run { config } => {
                assert_eq!(config, PathBuf::from("/tmp/roki.toml"));
            }
            _ => panic!("expected Run variant"),
        }
    }

    #[test]
    fn root_help_lists_run_subcommand() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(
            help.contains("run"),
            "root help should list `run` subcommand: {help}"
        );
    }

    #[test]
    fn run_help_names_config_and_roki_toml() {
        let cli = Cli::command();
        let run_cmd = cli.find_subcommand("run").expect("run subcommand exists");
        let help = run_cmd.clone().render_help().to_string();
        assert!(
            help.contains("--config"),
            "run help missing --config: {help}"
        );
        assert!(
            help.contains("roki.toml"),
            "run help should mention roki.toml: {help}"
        );
    }

    #[test]
    fn cleanup_subcommand_with_config_flag_parses() {
        let cli = Cli::try_parse_from(["roki", "cleanup", "--config", "/tmp/roki.toml"])
            .expect("should parse");
        match cli.command {
            CliCommand::Cleanup { config } => {
                assert_eq!(config, PathBuf::from("/tmp/roki.toml"));
            }
            _ => panic!("expected Cleanup variant"),
        }
    }

    #[test]
    fn root_help_lists_cleanup_subcommand() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(
            help.contains("cleanup"),
            "root help should list cleanup: {help}"
        );
    }
}
