//! Runtime bootstrap helpers.
//!
//! Task 1.1 provides the multi-threaded tokio runtime entry point and a
//! placeholder `run` handler that initializes tracing, emits a startup log
//! line, and exits. Task 1.3 will replace `init_tracing` with the
//! secret-redaction-aware tracing layer.

use anyhow::{Context, Result};
use tokio::runtime::Builder;
use tracing::info;

use crate::cli::RunArgs;

/// Build the multi-threaded tokio runtime used by the daemon.
///
/// Centralized so later tasks can adjust worker-thread count or instrumentation
/// in one place.
pub fn build_tokio_runtime() -> Result<tokio::runtime::Runtime> {
    Builder::new_multi_thread()
        .enable_all()
        .thread_name("roki-worker")
        .build()
        .context("failed to build tokio multi-threaded runtime")
}

/// Initialize a placeholder tracing subscriber.
///
/// This is intentionally minimal for task 1.1. Task 1.3 replaces this with the
/// redaction layer described in design.md (`logging.rs`).
pub fn init_tracing() {
    // `try_init` so duplicate initialization in tests does not panic.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

/// Execute `roki run`.
///
/// Task 1.1 scope: initialize tracing, emit a startup log line so operators
/// see the daemon came up, and exit cleanly. Subsequent tasks fold in the
/// orchestrator, tracker, workflow loader, and shutdown handling.
pub async fn run(_args: RunArgs) -> Result<()> {
    info!(version = env!("CARGO_PKG_VERSION"), "roki daemon starting");
    // Placeholder: later tasks build the orchestrator here and await shutdown.
    info!("roki daemon exiting cleanly (task 1.1 placeholder)");
    Ok(())
}
