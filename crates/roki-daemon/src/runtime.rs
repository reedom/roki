//! Runtime bootstrap helpers.
//!
//! Task 1.1 introduced the multi-threaded tokio runtime entry point. Task 1.3
//! replaces the placeholder `tracing_subscriber::fmt::init()` with the
//! redaction-aware tracing pipeline owned by [`crate::logging`]. The CLI shell
//! still calls [`init_tracing`] from `main.rs`; that wrapper now delegates to
//! the new layer with a stdout destination and the `info` filter as defaults
//! suitable for the bootstrap path. Once the configuration loader is wired
//! through `roki run` end-to-end (later tasks), [`run`] will rebuild the
//! logging pipeline from the loaded `Config` and the operator-declared
//! secrets.

use anyhow::{Context, Result};
use tokio::runtime::Builder;
use tracing::info;

use crate::cli::RunArgs;
use crate::logging::{LogContext, LoggingConfig, LoggingGuard};
use crate::shutdown::{ShutdownSignal, install_signal_handlers};

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

/// Initialize the bootstrap tracing pipeline.
///
/// This is invoked from `main.rs` before the configuration loader runs, so
/// the operator can see config-load errors. The pipeline is intentionally
/// minimal here: stdout destination, `info` filter, and an empty secret list.
/// Once `run` has loaded the config it can install the production pipeline
/// with the real secret list (Linear API token + operator-declared secrets).
///
/// Errors are non-fatal: a missing global subscriber is logged via stderr so
/// the binary can still boot. `try_init` allows tests in the same process to
/// race without panicking.
pub fn init_tracing() -> Option<LoggingGuard> {
    let directive = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let config = LoggingConfig::stdout(directive);
    match crate::logging::init(config) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("roki: tracing init failed: {error}");
            None
        }
    }
}

/// Execute `roki run`.
///
/// The pipeline is already initialized by `main.rs`. This entry point opens a
/// per-invocation context span (Requirement 12.2: `(repo, issue,
/// correlation_id)` fields are part of the standard event shape; for the
/// bootstrap log line we use a synthetic `daemon` repo and issue so the
/// startup events still carry the canonical context shape).
///
/// Subsequent tasks (1.5 multi-repo router, 2.x adapters) will build the
/// orchestrator here and pass it the [`ShutdownSignal`]. Task 1.4 wires the
/// signal-handling pipeline so SIGINT and SIGTERM trigger shutdown
/// observably; the bootstrap path itself simply awaits the signal and
/// returns.
pub async fn run(_args: RunArgs) -> Result<()> {
    let bootstrap_ctx = LogContext::new("daemon", "bootstrap", new_correlation_id());
    let _enter = bootstrap_ctx.span("daemon.bootstrap").entered();

    info!(version = env!("CARGO_PKG_VERSION"), "roki daemon starting");

    let shutdown = ShutdownSignal::new();
    let _signal_task = install_signal_handlers(shutdown.clone());

    // Until the orchestrator is wired (task 3.x) the bootstrap simply waits
    // for shutdown. Once tasks 1.5/3.x land, the orchestrator and adapters
    // take their own clones of `shutdown` and the bounded shutdown loop runs
    // their join handles through `await_workers_with_window`.
    shutdown.wait().await;

    info!("roki daemon exiting cleanly");
    Ok(())
}

/// Generate a fresh correlation identifier for a worker invocation or a
/// daemon-level event.
///
/// MVP keeps this simple: monotonic process-uptime nanoseconds rendered as
/// hex. Later tasks may swap in a UUID once we vendor a uuid crate. The
/// shape is opaque to consumers — only uniqueness within a daemon run
/// matters here.
fn new_correlation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("corr-{n:016x}")
}
