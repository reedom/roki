//! Bounded shutdown handling for the roki daemon (Requirement 1.3).
//!
//! Task 1.4 introduces:
//!
//! 1. [`ShutdownSignal`], a cloneable handle that propagates a single
//!    shutdown decision to every subsystem (orchestrator, adapters, workers)
//!    that subscribes to it. The signal is implemented over a
//!    [`tokio::sync::watch`] channel so multiple subscribers can `await` it
//!    independently without extra dependencies.
//! 2. [`install_signal_handlers`], which spawns a task that listens for
//!    `SIGINT` and (on Unix) `SIGTERM` and triggers the signal exactly once.
//! 3. [`await_workers_with_window`], the bounded shutdown loop. It waits for
//!    each worker join handle up to a per-window deadline and then forces
//!    abort, ensuring the daemon exits cleanly within a documented window.
//!
//! Per the design's File Structure Plan, this module lives at
//! `crates/roki-daemon/src/shutdown.rs` and exposes an API that the
//! orchestrator (task 3.2) and adapters can depend on additively without
//! reaching back into the runtime bootstrap.

use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Default bounded shutdown window applied per worker join in
/// [`await_workers_with_window`] when no caller-provided window is supplied.
///
/// The value is currently a public constant rather than a config-derived
/// value to avoid bleeding the task 1.2 configuration boundary into 1.4.
/// Once the orchestrator (3.x) is wired through `Config`, this constant
/// becomes the default and the loaded `Config` may override it. Downstream
/// adapters that need the canonical "documented window" should bind to this
/// constant for now.
pub const SHUTDOWN_WINDOW: Duration = Duration::from_secs(30);

/// Single shutdown decision propagated through the orchestrator and adapters.
///
/// Cloneable: every subsystem that needs to react to shutdown holds its own
/// clone. Internally the signal uses a [`tokio::sync::watch`] channel whose
/// `bool` value flips from `false` to `true` exactly once. After `trigger`
/// the signal stays in the shutting-down state for the rest of the process
/// lifetime.
#[derive(Debug, Clone)]
pub struct ShutdownSignal {
    tx: watch::Sender<bool>,
    rx: watch::Receiver<bool>,
}

impl ShutdownSignal {
    /// Construct a fresh shutdown signal.
    ///
    /// The signal starts in the not-shutting-down state.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx, rx }
    }

    /// Trigger shutdown. Idempotent: subsequent calls are no-ops.
    ///
    /// Returns `true` when this call flipped the signal, `false` when the
    /// signal had already been triggered. Used by the OS-signal handler and
    /// by tests.
    pub fn trigger(&self) -> bool {
        let mut flipped = false;
        // `send_modify` runs the closure even when there are no receivers,
        // which is what we want for the test-process case.
        self.tx.send_modify(|state| {
            if !*state {
                *state = true;
                flipped = true;
            }
        });
        if flipped {
            info!("shutdown signal triggered");
        }
        flipped
    }

    /// Returns whether shutdown has been triggered.
    ///
    /// Adapters consult this on the "accept new work" path so they can refuse
    /// new work as soon as shutdown begins. Per Requirement 1.3, the daemon
    /// must stop accepting new work on shutdown.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        *self.rx.borrow()
    }

    /// Await the shutdown signal.
    ///
    /// Returns immediately if shutdown has already been triggered.
    pub async fn wait(&self) {
        let mut rx = self.rx.clone();
        // Fast path: already shutting down.
        if *rx.borrow() {
            return;
        }
        // Wait for any change. The only legal change is `false -> true`, and
        // a sender drop is treated as "shutdown" so the daemon does not hang
        // if its trigger handle is dropped early.
        let _ = rx.changed().await;
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn a task that listens for `SIGINT` (and `SIGTERM` on Unix) and
/// triggers the supplied [`ShutdownSignal`] exactly once.
///
/// On non-Unix targets only `Ctrl-C` is observed. SIGTERM is gated behind
/// `#[cfg(unix)]`. Windows is out of scope for the MVP, but the gating keeps
/// the build working there.
///
/// The returned [`JoinHandle`] resolves once the listener observes a signal
/// and triggers shutdown. Most callers can ignore the handle; tests may
/// `.await` it to assert the listener responded.
pub fn install_signal_handlers(signal: ShutdownSignal) -> JoinHandle<()> {
    tokio::spawn(async move {
        wait_for_os_signal().await;
        if signal.trigger() {
            debug!("shutdown listener flipped signal");
        } else {
            debug!("shutdown listener observed signal but signal was already set");
        }
    })
}

/// Platform-specific OS signal wait.
///
/// On Unix, races SIGINT and SIGTERM and resolves on whichever arrives first.
/// On other platforms, only SIGINT (Ctrl-C) is observed.
async fn wait_for_os_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(error) => {
                warn!(%error, "failed to install SIGTERM handler; falling back to SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT");
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("received Ctrl-C");
    }
}

/// Outcome of a bounded shutdown await across a set of workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShutdownOutcome {
    /// Number of worker join handles that completed cooperatively within the
    /// window.
    pub completed: usize,
    /// Number of worker join handles that exceeded the window and had to be
    /// force-aborted.
    pub timed_out: usize,
}

/// Await each worker handle up to `window`, then force-abort any that have
/// not completed.
///
/// This is the bounded shutdown loop required by Requirement 1.3: the daemon
/// signals each active worker to terminate, awaits a bounded shutdown window
/// per worker, and exits cleanly. Workers that ignore the signal are aborted
/// when the window elapses; workers that exit cooperatively shorten the
/// total wait below the window.
///
/// Returns a [`ShutdownOutcome`] summarizing how many workers completed and
/// how many were force-aborted, so callers can log the decision.
pub async fn await_workers_with_window<T>(
    workers: Vec<JoinHandle<T>>,
    window: Duration,
) -> ShutdownOutcome
where
    T: Send + 'static,
{
    let mut outcome = ShutdownOutcome::default();
    for worker in workers {
        match tokio::time::timeout(window, &mut wait_handle(worker)).await {
            Ok(()) => outcome.completed = outcome.completed.saturating_add(1),
            Err(_) => outcome.timed_out = outcome.timed_out.saturating_add(1),
        }
    }
    outcome
}

/// Await a single join handle and force-abort it if the future is dropped
/// before completion.
///
/// Wrapped as a small helper so [`await_workers_with_window`] can use
/// `tokio::time::timeout` without losing access to the handle for the
/// force-abort path.
fn wait_handle<T>(handle: JoinHandle<T>) -> WaitHandle<T>
where
    T: Send + 'static,
{
    WaitHandle {
        handle: Some(handle),
    }
}

/// Future that awaits a join handle and aborts it on drop if it has not
/// resolved.
///
/// `tokio::time::timeout(window, &mut WaitHandle)` therefore yields the
/// "force abort after window" semantics required by Requirement 1.3.
struct WaitHandle<T> {
    handle: Option<JoinHandle<T>>,
}

impl<T> std::future::Future for WaitHandle<T>
where
    T: Send + 'static,
{
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let Some(handle) = self.handle.as_mut() else {
            return std::task::Poll::Ready(());
        };
        let pinned = std::pin::Pin::new(handle);
        match std::future::Future::poll(pinned, cx) {
            std::task::Poll::Ready(_) => {
                // Worker completed (cleanly or with a panic). Either way the
                // shutdown loop counts this as "completed within window".
                self.handle = None;
                std::task::Poll::Ready(())
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<T> Drop for WaitHandle<T> {
    fn drop(&mut self) {
        // If the timeout expired, the join handle is still here and the
        // worker has not exited cooperatively. Force-abort it so the daemon
        // can exit. Workers that already completed have `self.handle == None`
        // and are unaffected.
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trigger_is_idempotent() {
        let signal = ShutdownSignal::new();
        assert!(signal.trigger(), "first trigger should flip the signal");
        assert!(!signal.trigger(), "second trigger should be a no-op");
        assert!(signal.is_shutting_down());
    }

    #[tokio::test]
    async fn wait_resolves_immediately_when_already_triggered() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        // If `wait` did not short-circuit, this would hang forever because
        // `changed()` only fires on subsequent flips.
        tokio::time::timeout(Duration::from_millis(100), signal.wait())
            .await
            .expect("wait must short-circuit when already shutting down");
    }

    #[tokio::test]
    async fn await_workers_window_returns_completed_count() {
        let handles = vec![
            tokio::spawn(async {}),
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }),
        ];
        let outcome = await_workers_with_window(handles, Duration::from_secs(1)).await;
        assert_eq!(outcome.completed, 2);
        assert_eq!(outcome.timed_out, 0);
    }
}
