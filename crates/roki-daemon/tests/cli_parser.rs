//! Integration tests for the task 1.1 CLI surface.
//!
//! These tests cover the observable contract from requirements 1.1 and 1.5:
//!   - `roki --help` documents subcommands (clap renders this from the
//!     `Cli` derive metadata; we assert the metadata names `run`).
//!   - `roki run` parses without arguments and resolves to `Command::Run`.
//!   - Invocations with no subcommand fail (clap error), so the binary
//!     cannot accidentally start an unconfigured daemon.

use clap::{CommandFactory, Parser};
use roki_daemon::cli::{Cli, Command};

#[test]
fn parses_run_subcommand() {
    let cli = Cli::try_parse_from(["roki", "run"]).expect("`roki run` should parse");
    match cli.command {
        Command::Run(_) => {}
    }
}

#[test]
fn missing_subcommand_is_an_error() {
    let result = Cli::try_parse_from(["roki"]);
    assert!(
        result.is_err(),
        "invoking `roki` with no subcommand must fail rather than silently default"
    );
}

#[test]
fn help_output_documents_run_subcommand() {
    let mut command = Cli::command();
    let help = command.render_long_help().to_string();
    assert!(
        help.contains("run"),
        "top-level --help must document the `run` subcommand; got:\n{help}"
    );
}

#[test]
fn run_subcommand_help_renders() {
    let result = Cli::try_parse_from(["roki", "run", "--help"]);
    let error = result.expect_err("--help should short-circuit parsing");
    // clap reports --help as a non-failure error kind; assert that shape so a
    // future regression that turns help into a real parse error is caught.
    assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
}
