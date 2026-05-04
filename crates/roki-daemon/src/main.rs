//! `roki` binary entry point.
//!
//! Tasks 1.1 + 1.2: bootstrap a tokio multi-thread runtime and dispatch on
//! the parsed CLI subcommand. Subcommand bodies (config load, runtime
//! composition) are filled in by tasks 1.3 onward.

use std::process::ExitCode;

use roki_daemon::cli::{Cli, Command};
use roki_daemon::runtime;

fn main() -> ExitCode {
    let cli = Cli::parse_from_env();

    let async_runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("roki: failed to build tokio runtime: {error}");
            return ExitCode::from(1);
        }
    };

    let result = async_runtime.block_on(async move {
        match cli.command {
            Command::Run(args) => runtime::run(args).await,
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("roki: {err}");
            ExitCode::from(1)
        }
    }
}
