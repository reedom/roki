//! Filesystem watcher with debounce + last-known-good fallback for
//! `WORKFLOW.md` (Requirements 6.3, 6.4).
//!
//! The handle wraps a tokio task that bridges the synchronous
//! `notify-debouncer-mini` channel into the async runtime. On every debounced
//! event the file is re-parsed and re-validated:
//!
//! * On success the in-memory policy is replaced atomically (a fresh `Arc` is
//!   stored under `tokio::sync::watch`).
//! * On failure the previous valid policy is retained (last-known-good) and a
//!   `tracing::warn!` event is emitted with structured fields naming the
//!   offending key path.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer};
use tokio::sync::watch;

use super::{WorkflowLoader, WorkflowPolicy};

/// Errors raised while setting up or tearing down the watcher.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("initial WORKFLOW.md load failed: {0}")]
    InitialLoad(#[source] super::WorkflowError),

    #[error("filesystem watcher failed: {0}")]
    Notify(#[from] notify::Error),
}

/// Handle to a running watcher. Drop the handle to stop watching.
///
/// `current()` always returns the most recently validated policy. The
/// underlying [`watch::Receiver`] can be exposed in future tasks if a
/// subscribe-style API is required by the orchestrator; for task 2.3 the
/// snapshot accessor is enough.
pub struct WorkflowHandle {
    rx: watch::Receiver<Arc<WorkflowPolicy>>,
    /// Owns the debouncer + bridge task; dropping stops both. The fields are
    /// held so their `Drop` impls run; we never read them.
    _shutdown: WatchShutdown,
}

struct WatchShutdown {
    debouncer: Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
    bridge: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for WatchShutdown {
    fn drop(&mut self) {
        // Drop the debouncer first so its background thread stops emitting,
        // then abort the bridge task so it does not hold the watch sender.
        let _ = self.debouncer.take();
        if let Some(handle) = self.bridge.take() {
            handle.abort();
        }
    }
}

impl WorkflowHandle {
    pub(super) async fn spawn(
        path: PathBuf,
        initial: Arc<WorkflowPolicy>,
        debounce: Duration,
    ) -> Result<Self, WatchError> {
        let (tx, rx) = watch::channel(initial);

        // Bridge synchronous debouncer events into the async runtime via a
        // tokio mpsc channel.
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::unbounded_channel::<DebounceEventResult>();

        let mut debouncer = new_debouncer(debounce, move |res: DebounceEventResult| {
            // The receiving task may have shut down; ignoring send errors is
            // intentional — we are tearing down anyway.
            let _ = event_tx.send(res);
        })?;

        // Watch the parent directory recursively so atomic-rename writes
        // (mv tmp WORKFLOW.md) and editor save patterns are still observed.
        let watch_target = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        debouncer
            .watcher()
            .watch(&watch_target, RecursiveMode::NonRecursive)?;

        let bridge_path = path.clone();
        // Compare against the file name as a fallback. Some platforms
        // (notably macOS FSEvents on a renamed temp -> target) report event
        // paths whose canonical form differs from the watched path string,
        // so we accept any event under the watched directory whose final
        // component matches our target file name.
        let target_file_name = path.file_name().map(|n| n.to_os_string());
        let bridge = tokio::spawn(async move {
            while let Some(result) = event_rx.recv().await {
                let touched = match result {
                    Ok(events) => events.iter().any(|ev| {
                        ev.path == bridge_path
                            || target_file_name
                                .as_ref()
                                .is_some_and(|name| ev.path.file_name() == Some(name.as_os_str()))
                    }),
                    Err(error) => {
                        tracing::warn!(
                            target = "roki.workflow",
                            event = "watcher_error",
                            error = %error,
                            "filesystem watcher reported an error",
                        );
                        false
                    }
                };
                if !touched {
                    continue;
                }
                match WorkflowLoader::load(&bridge_path) {
                    Ok(policy) => {
                        if tx.send(Arc::new(policy)).is_err() {
                            // No more receivers; handle was dropped.
                            break;
                        }
                        tracing::info!(
                            target = "roki.workflow",
                            event = "workflow_reloaded",
                            path = %bridge_path.display(),
                            "WORKFLOW.md reloaded successfully",
                        );
                    }
                    Err(err) => {
                        // Last-known-good fallback (Requirement 6.4): retain
                        // the prior valid policy and emit a structured warn
                        // event identifying the offending key path.
                        let key_path = err.key_path().unwrap_or("<root>").to_string();
                        tracing::warn!(
                            target = "roki.workflow",
                            event = "workflow_validation_failed",
                            path = %bridge_path.display(),
                            key_path = %key_path,
                            reason = %err,
                            "WORKFLOW.md reload failed validation; retaining last-known-good policy",
                        );
                    }
                }
            }
        });

        Ok(Self {
            rx,
            _shutdown: WatchShutdown {
                debouncer: Some(debouncer),
                bridge: Some(bridge),
            },
        })
    }

    /// Snapshot of the most recently validated policy. Replaced atomically on
    /// every successful reload.
    pub fn current(&self) -> Arc<WorkflowPolicy> {
        self.rx.borrow().clone()
    }

    /// Wait for the next successful reload (used by tests + future
    /// orchestrator wiring). The boolean is `Ok(true)` when a new policy
    /// arrived, `Ok(false)` if the watcher was shut down before any reload.
    pub async fn changed(&mut self) -> Result<bool, watch::error::RecvError> {
        match self.rx.changed().await {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}
