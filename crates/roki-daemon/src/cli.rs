//! Top-level CLI parser for the `roki` binary.
//!
//! Task 1.2 scope: declare the `roki run` subcommand with the five canonical
//! flags from [`docs/reference/cli.md`](../../../../docs/reference/cli.md):
//! `--config`, `--bind`, `--port`, `--dangerously-skip-permissions`, `--debug`.
//! Each flag's `--help` text names the configuration key it overrides per
//! roki-mvp requirement 1.6. Subsequent tasks consume `RunArgs` and merge it
//! into the loaded configuration so CLI values win over config-file values.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Top-level argument parser. Every binding is `Option<T>` so the runtime can
/// distinguish "operator did not pass this flag" from "operator passed an
/// explicit value." The configuration loader applies file-level defaults to
/// any field left `None`.
#[derive(Debug, Parser)]
#[command(
    name = "roki",
    version,
    about = "roki daemon: Linear-driven, per-issue agent orchestrator",
    long_about = "roki observes Linear, supervises a long-lived orchestrator session and \
                  short-lived phase subprocesses per admitted ticket, and reconciles per-issue \
                  state on restart. The daemon never writes Linear, never opens PRs, and never \
                  edits source files."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon (orchestrator runtime, Linear adapter, workflow loader,
    /// webhook server).
    Run(RunArgs),
}

/// Arguments to `roki run`. Each flag corresponds to a configuration key it
/// overrides; the column is documented in
/// [`docs/reference/cli.md`](../../../../docs/reference/cli.md).
#[derive(Debug, Default, Clone, Parser, PartialEq, Eq)]
pub struct RunArgs {
    /// Path to `roki.toml` (overrides the documented default search order).
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Webhook receiver bind address (overrides `[server].bind`).
    #[arg(long = "bind", value_name = "ADDR")]
    pub bind: Option<String>,

    /// Webhook receiver bind port (overrides `[server].port`).
    #[arg(long = "port", value_name = "NUM")]
    pub port: Option<u16>,

    /// Pin the entire phase-subprocess permission strategy to
    /// `--dangerously-skip-permissions` (fallback for when Claude Code's
    /// allowlist cannot be trusted). Does NOT apply to the orchestrator
    /// session, which always runs read-only.
    #[arg(long = "dangerously-skip-permissions", default_value_t = false)]
    pub dangerously_skip_permissions: bool,

    /// Enable per-issue debug capture (records each subprocess's stdout/stderr
    /// to a per-issue file).
    #[arg(long = "debug", default_value_t = false)]
    pub debug: bool,
}

impl Cli {
    /// Parse from process-level `std::env::args_os`.
    pub fn parse_from_env() -> Self {
        Self::parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    fn parse_run(args: &[&str]) -> RunArgs {
        let mut full = vec!["roki", "run"];
        full.extend_from_slice(args);
        match Cli::parse_from(full).command {
            Command::Run(run) => run,
        }
    }

    #[test]
    fn run_parses_all_five_flags() {
        let parsed = parse_run(&[
            "--config",
            "/etc/roki/roki.toml",
            "--bind",
            "0.0.0.0",
            "--port",
            "9000",
            "--dangerously-skip-permissions",
            "--debug",
        ]);
        assert_eq!(parsed.config, Some(PathBuf::from("/etc/roki/roki.toml")));
        assert_eq!(parsed.bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(parsed.port, Some(9000));
        assert!(parsed.dangerously_skip_permissions);
        assert!(parsed.debug);
    }

    #[test]
    fn run_with_no_flags_yields_all_defaults() {
        let parsed = parse_run(&[]);
        assert_eq!(parsed, RunArgs::default());
        assert!(parsed.config.is_none());
        assert!(parsed.bind.is_none());
        assert!(parsed.port.is_none());
        assert!(!parsed.dangerously_skip_permissions);
        assert!(!parsed.debug);
    }

    #[test]
    fn run_individual_flags_round_trip() {
        assert_eq!(
            parse_run(&["--config", "x.toml"]).config,
            Some(PathBuf::from("x.toml"))
        );
        assert_eq!(parse_run(&["--bind", "127.0.0.1"]).bind.as_deref(), Some("127.0.0.1"));
        assert_eq!(parse_run(&["--port", "1"]).port, Some(1));
        assert!(parse_run(&["--dangerously-skip-permissions"]).dangerously_skip_permissions);
        assert!(parse_run(&["--debug"]).debug);
    }

    #[test]
    fn run_rejects_invalid_port() {
        let result = Cli::try_parse_from(["roki", "run", "--port", "not-a-number"]);
        assert!(result.is_err(), "non-numeric port must be rejected");
    }

    #[test]
    fn root_help_lists_run_subcommand() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(help.contains("run"), "root --help must list `run` subcommand");
    }

    #[test]
    fn run_help_documents_every_flag() {
        let cli = Cli::command();
        let mut run = cli
            .find_subcommand("run")
            .expect("`run` subcommand registered")
            .clone();
        let help = run.render_help().to_string();
        for flag in [
            "--config",
            "--bind",
            "--port",
            "--dangerously-skip-permissions",
            "--debug",
        ] {
            assert!(help.contains(flag), "run --help must document {flag}");
        }
    }
}
