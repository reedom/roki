//! `roki` binary entry point.
//!
//! Task 1.1 scope: bootstrap a tokio multi-thread runtime and hand control
//! to the CLI shell. The CLI surface itself is fleshed out in task 1.2.

use std::process::ExitCode;

use clap::Parser;

/// Top-level CLI parser. Subcommands are added by task 1.2.
#[derive(Debug, Parser)]
#[command(
    name = "roki",
    version,
    about = "roki daemon: Linear-driven, per-issue agent orchestrator"
)]
struct Cli {
    /// Reserved subcommand surface; populated by task 1.2.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {}

fn main() -> ExitCode {
    let _cli = Cli::parse();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("roki: failed to build tokio runtime: {error}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async {});
    ExitCode::SUCCESS
}
