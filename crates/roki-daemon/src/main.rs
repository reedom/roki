//! Binary entry point for the `roki` daemon.
//!
//! Responsibilities for task 1.1:
//! 1. Parse CLI arguments via clap.
//! 2. Bootstrap a tokio multi-threaded runtime.
//! 3. Dispatch to the selected subcommand handler.
//!
//! The runtime is built before tracing is initialized so that tracing setup
//! itself can run inside async context in later tasks (e.g., reading a config
//! file asynchronously). For task 1.1 the order is irrelevant; the layout is
//! chosen for forward compatibility.

use std::process::ExitCode;

use roki_daemon::cli::{Cli, Command};
use roki_daemon::runtime::{build_tokio_runtime, init_tracing, run};

fn main() -> ExitCode {
    let cli = Cli::parse_from_env();

    let runtime = match build_tokio_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("roki: {error:#}");
            return ExitCode::from(1);
        }
    };

    // Hold the guard for the duration of the run so any log file flushes on
    // shutdown. Task 1.3 introduced the redaction-aware pipeline that returns
    // this guard; previous tasks discarded the return value of the placeholder
    // initializer.
    let _logging_guard = init_tracing();

    let result = runtime.block_on(async {
        match cli.command {
            Command::Run(args) => run(args).await,
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("roki: {error:#}");
            ExitCode::from(1)
        }
    }
}
