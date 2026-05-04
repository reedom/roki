//! `roki` binary entry point.
//!
//! Tasks 1.1 + 1.2: bootstrap a tokio multi-thread runtime and dispatch on
//! the parsed CLI subcommand. Subcommand bodies (config load, runtime
//! composition) are filled in by tasks 1.3 onward.

use std::process::ExitCode;

use roki_daemon::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse_from_env();

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

    runtime.block_on(async {
        match cli.command {
            Command::Run(_args) => {
                // Task 1.5+ wires the actual runtime composition. For task 1.2
                // the binary exits cleanly so `cargo run -- run --help` (which
                // exits before reaching this point) and CLI-only smoke tests
                // do not need a fully-wired daemon.
            }
        }
    });

    ExitCode::SUCCESS
}
