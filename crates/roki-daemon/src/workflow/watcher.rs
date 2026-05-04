//! Hot reload watcher for `WORKFLOW.md`.
//!
//! Contract (Req 6.3): a hot-reload swap only affects the next ticket
//! admission. An in-flight orchestrator keeps its rendered system prompt and
//! an in-flight phase keeps its rendered prompt for the lifetime of the
//! session. This is enforced upstream by capturing the current
//! `Arc<WorkflowPolicy>` at orchestrator launch / phase nomination; this
//! module is responsible only for swapping the shared atomic handle.
//!
//! On validation failure (Req 6.4) the watcher retains the last-known-good
//! policy and logs the error with the offending key path. On success it
//! installs the new policy via [`tokio::sync::RwLock::write`].
//!
//! Spec refs: requirements.md Req 6.3, 6.4.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender as StdSender;
use std::time::Duration;

use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use thiserror::Error;
use tokio::fs;
use tokio::sync::{RwLock, mpsc};

use super::parse::{ParseError, parse_str};
use super::schema::{SchemaError, WorkflowPolicy, validate};
use crate::shutdown::ShutdownSignal;

/// Default debounce window for production. Tests pass a much shorter value
/// via [`spawn_with_debounce`].
pub const DEFAULT_DEBOUNCE_WINDOW: Duration = Duration::from_millis(200);

/// Background-task return shape; consumers typically ignore the value and
/// rely on the [`tokio::task::JoinHandle`] to know when the watcher exited.
pub struct WorkflowWatcher {
    pub policy: Arc<RwLock<Arc<WorkflowPolicy>>>,
    pub join: tokio::task::JoinHandle<()>,
    /// Holds the OS-level watcher alive. Dropping it stops file events.
    _debouncer: Debouncer<RecommendedWatcher>,
}

#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("failed to read WORKFLOW.md at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse WORKFLOW.md: {0}")]
    Parse(#[from] ParseError),

    #[error("failed to validate WORKFLOW.md: {0}")]
    Schema(#[from] SchemaError),

    #[error("failed to install filesystem watcher on {path}: {source}")]
    NotifySetup {
        path: PathBuf,
        #[source]
        source: notify_debouncer_mini::notify::Error,
    },
}

/// Read, parse, and validate the file at `path`. Used both at initial boot
/// and on every hot-reload.
pub async fn load_policy(path: &Path) -> Result<WorkflowPolicy, WatcherError> {
    let text = fs::read_to_string(path).await.map_err(|source| WatcherError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed = parse_str(&text)?;
    let policy = validate(parsed)?;
    Ok(policy)
}

/// Spawn the hot-reload watcher with the production debounce window.
pub async fn spawn(
    path: PathBuf,
    initial: WorkflowPolicy,
    shutdown: ShutdownSignal,
) -> Result<WorkflowWatcher, WatcherError> {
    spawn_with_debounce(path, initial, shutdown, DEFAULT_DEBOUNCE_WINDOW).await
}

/// Like [`spawn`] but with a caller-controlled debounce window. Tests use
/// `~50ms` so they do not wait on the production 200ms default.
pub async fn spawn_with_debounce(
    path: PathBuf,
    initial: WorkflowPolicy,
    shutdown: ShutdownSignal,
    debounce: Duration,
) -> Result<WorkflowWatcher, WatcherError> {
    let policy = Arc::new(RwLock::new(Arc::new(initial)));

    // The debouncer hands us a `std::sync::mpsc::Sender`; we bridge into a
    // tokio `mpsc` so the async reload loop is `Send` (a sync `Receiver` is
    // !Sync and cannot cross an `.await` inside a tokio task).
    let (std_tx, std_rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let (tokio_tx, tokio_rx) = mpsc::channel::<DebounceEventResult>(32);

    let mut debouncer = new_debouncer(debounce, EventForwarder::new(std_tx)).map_err(|source| {
        WatcherError::NotifySetup {
            path: path.clone(),
            source,
        }
    })?;
    debouncer
        .watcher()
        .watch(&path, RecursiveMode::NonRecursive)
        .map_err(|source| WatcherError::NotifySetup {
            path: path.clone(),
            source,
        })?;

    // Bridge thread: blocking-recv from the std channel, push to tokio. The
    // thread exits naturally when the debouncer drops its sender.
    std::thread::spawn(move || {
        while let Ok(event) = std_rx.recv() {
            if tokio_tx.blocking_send(event).is_err() {
                break;
            }
        }
    });

    let policy_handle = Arc::clone(&policy);
    let watch_path = path.clone();
    let join = tokio::spawn(reload_loop(tokio_rx, policy_handle, watch_path, shutdown));

    Ok(WorkflowWatcher {
        policy,
        join,
        _debouncer: debouncer,
    })
}

/// Forwarder hands debounced events from the OS thread into our channel.
struct EventForwarder {
    tx: StdSender<DebounceEventResult>,
}

impl EventForwarder {
    fn new(tx: StdSender<DebounceEventResult>) -> Self {
        Self { tx }
    }
}

impl notify_debouncer_mini::DebounceEventHandler for EventForwarder {
    fn handle_event(&mut self, event: DebounceEventResult) {
        // Best-effort: receiver gone means the daemon has shut down — drop.
        let _ = self.tx.send(event);
    }
}

async fn reload_loop(
    mut rx: mpsc::Receiver<DebounceEventResult>,
    policy: Arc<RwLock<Arc<WorkflowPolicy>>>,
    path: PathBuf,
    shutdown: ShutdownSignal,
) {
    loop {
        tokio::select! {
            _ = shutdown.wait() => return,
            event = rx.recv() => {
                match event {
                    None => return,
                    Some(Err(error)) => {
                        tracing::warn!(error = ?error, "WORKFLOW.md watcher reported error event");
                    }
                    Some(Ok(_events)) => {
                        handle_reload(&path, &policy).await;
                    }
                }
            }
        }
    }
}

async fn handle_reload(path: &Path, policy: &Arc<RwLock<Arc<WorkflowPolicy>>>) {
    match load_policy(path).await {
        Ok(new_policy) => {
            let mut guard = policy.write().await;
            *guard = Arc::new(new_policy);
            tracing::info!(path = %path.display(), "WORKFLOW.md hot reload applied");
        }
        Err(error) => {
            // Req 6.4: keep the previous policy, log with the offending key
            // path embedded in the error message.
            tracing::error!(
                error = %error,
                path = %path.display(),
                "WORKFLOW.md hot reload failed; retaining previous policy",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shutdown;
    use std::time::Duration;
    use tempfile::TempDir;

    const VALID_BODY: &str = "---\nextension:\n  orchestrator:\n    max_phases: 5\n---\n\
        ## prompt_template_orchestrator\norch v1\n\
        \n## prompt_template_implement_direct\nimpl\n\
        \n## prompt_template_validate_direct\nval\n\
        \n## prompt_template_open_pr\nopen\n";

    const VALID_BODY_V2: &str = "---\nextension:\n  orchestrator:\n    max_phases: 9\n---\n\
        ## prompt_template_orchestrator\norch v2\n\
        \n## prompt_template_implement_direct\nimpl\n\
        \n## prompt_template_validate_direct\nval\n\
        \n## prompt_template_open_pr\nopen\n";

    const INVALID_BODY: &str = "---\nextension:\n  orchestrator:\n    max_phases: 999\n---\n\
        ## prompt_template_orchestrator\norch invalid\n\
        \n## prompt_template_implement_direct\nimpl\n\
        \n## prompt_template_validate_direct\nval\n\
        \n## prompt_template_open_pr\nopen\n";

    async fn wait_for<F>(mut predicate: F, timeout: Duration) -> bool
    where
        F: FnMut() -> Option<bool>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(true) = predicate() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_reload_retains_previous_policy_then_valid_reload_swaps() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        std::fs::write(&path, VALID_BODY).unwrap();

        let initial = load_policy(&path).await.expect("initial load");
        assert_eq!(initial.orchestrator.max_phases, 5);

        let (signal, trigger) = shutdown::new();
        let watcher = spawn_with_debounce(
            path.clone(),
            initial,
            signal.clone(),
            Duration::from_millis(50),
        )
        .await
        .expect("spawn watcher");

        // (a) Initial policy installed.
        {
            let guard = watcher.policy.read().await;
            assert_eq!(guard.orchestrator.max_phases, 5);
        }

        // (b) Write invalid → previous retained.
        std::fs::write(&path, INVALID_BODY).unwrap();
        // The watcher should observe the change but reject; previous policy
        // remains. Sleep a bit longer than the debounce window plus poll
        // cadence to guarantee the failure path ran.
        tokio::time::sleep(Duration::from_millis(400)).await;
        {
            let guard = watcher.policy.read().await;
            assert_eq!(
                guard.orchestrator.max_phases, 5,
                "previous policy must be retained on invalid reload",
            );
        }

        // (c) Write valid v2 → new policy applies.
        std::fs::write(&path, VALID_BODY_V2).unwrap();
        let swapped = wait_for(
            || {
                let policy = watcher.policy.try_read().ok()?;
                Some(policy.orchestrator.max_phases == 9)
            },
            Duration::from_secs(3),
        )
        .await;
        assert!(swapped, "valid reload must swap policy within timeout");

        // Tear down.
        trigger.fire();
        // Drop watcher so debouncer's thread exits and reload_loop sees end.
        drop(watcher);
        // Wait briefly; we are not asserting any timing here.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
