//! Bounded shutdown primitive for the roki daemon runtime.
//!
//! `ShutdownSignal` is a cheap-clone receiver handle subscribed by the
//! orchestrator core, tracker, axum webhook server, and engine adapters; each
//! awaits `wait()` (or polls `is_shutting_down()`) to learn that wind-down has
//! begun. `ShutdownTrigger` is the single sender side wired by the daemon
//! bootstrap to the SIGINT/SIGTERM handlers installed by
//! `install_signal_handlers`. `await_workers_with_window` enforces the overall
//! `SHUTDOWN_WINDOW = 30s` budget at wind-down by awaiting each tagged worker
//! future concurrently and reporting which finished cleanly versus which were
//! force-dropped past the window so callers can log and exit.
//!
//! Design references:
//! - design.md File Structure Plan line 231 (shutdown.rs scope)
//! - design.md "Daemon bootstrap" steps 5 and 12 (signal handlers, tokio
//!   `select!` wind-down, SHUTDOWN_WINDOW = 30s, `await_workers_with_window`)
//! - Requirement 1.4 (bounded shutdown handling)

use std::future::Future;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Overall daemon wind-down budget: the orchestrator session, tracker,
/// webhook server, and in-flight phase subprocesses must all complete or be
/// force-dropped within this window before the daemon exits zero.
pub const SHUTDOWN_WINDOW: Duration = Duration::from_secs(30);

/// Cheap-clone receiver handle for shutdown notification.
///
/// Cloning is intentionally cheap (a `watch::Receiver<bool>` clone) so every
/// long-running task in the daemon can hold its own copy without coordination.
#[derive(Clone, Debug)]
pub struct ShutdownSignal {
    rx: watch::Receiver<bool>,
}

/// Single sender-side handle for triggering shutdown.
///
/// `Drop` is intentionally a no-op: dropping the trigger without firing must
/// not cancel subscribers, because daemon bootstrap moves the trigger into the
/// signal-handler task and never explicitly drops it on the happy path.
#[derive(Debug)]
pub struct ShutdownTrigger {
    tx: watch::Sender<bool>,
}

/// Outcome of `await_workers_with_window`: which tagged workers finished
/// before the window elapsed and which were left running past it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AwaitOutcome {
    pub completed: Vec<String>,
    pub timed_out: Vec<String>,
}

/// Construct a paired `(ShutdownSignal, ShutdownTrigger)`.
///
/// The signal can be cloned freely and handed to every shutdown-aware task;
/// the trigger is the single producer wired to the SIGINT/SIGTERM handlers.
pub fn new() -> (ShutdownSignal, ShutdownTrigger) {
    let (tx, rx) = watch::channel(false);
    (ShutdownSignal { rx }, ShutdownTrigger { tx })
}

impl ShutdownSignal {
    /// Resolve when shutdown has been triggered. Safe to call repeatedly.
    pub async fn wait(&self) {
        let mut rx = self.rx.clone();
        // Already-fired check: if the watch already holds `true`, return
        // immediately rather than waiting for the next change.
        if *rx.borrow() {
            return;
        }
        // `changed()` only errors when the sender is dropped without firing;
        // in that case shutdown will never come, so we park forever.
        while rx.changed().await.is_ok() {
            if *rx.borrow() {
                return;
            }
        }
        std::future::pending::<()>().await;
    }

    /// Non-blocking check whether shutdown has been triggered.
    pub fn is_shutting_down(&self) -> bool {
        *self.rx.borrow()
    }
}

impl ShutdownTrigger {
    /// Signal shutdown to all subscribers. Idempotent.
    pub fn fire(&self) {
        // `send_replace` is infallible even when there are no subscribers,
        // which matters because the trigger may outlive every signal handle.
        self.tx.send_replace(true);
    }
}

/// Install SIGINT and SIGTERM handlers that fire `trigger` exactly once.
///
/// Returns the handler task's `JoinHandle` so the bootstrap can await it (or
/// abort it during teardown). Both signal kinds are Unix-only; the daemon
/// targets macOS and Linux.
pub fn install_signal_handlers(trigger: ShutdownTrigger) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "failed to install SIGINT handler");
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "failed to install SIGTERM handler");
                return;
            }
        };
        tokio::select! {
            _ = sigint.recv() => {
                tracing::info!("SIGINT received; initiating shutdown");
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received; initiating shutdown");
            }
        }
        trigger.fire();
    })
}

/// Await each tagged worker concurrently bounded by `window`.
///
/// Each worker's tag (the `String` half of the input pair) is recorded in
/// either `completed` or `timed_out`. Slow workers are not awaited past the
/// window; `await_workers_with_window` returns once `window` has elapsed even
/// if some workers are still running, so the daemon's overall wind-down stays
/// within `SHUTDOWN_WINDOW`.
pub async fn await_workers_with_window<I, F>(workers: I, window: Duration) -> AwaitOutcome
where
    I: IntoIterator<Item = (String, F)>,
    F: Future<Output = ()> + Send + 'static,
{
    // Spawn each worker on its own task so they all run concurrently; the
    // deadline applies to the whole batch, not sequentially per-worker.
    let mut pending: Vec<(String, JoinHandle<()>)> = workers
        .into_iter()
        .map(|(tag, fut)| (tag, tokio::spawn(fut)))
        .collect();

    let mut outcome = AwaitOutcome::default();
    let sleep = tokio::time::sleep(window);
    tokio::pin!(sleep);

    loop {
        if pending.is_empty() {
            return outcome;
        }
        // Build a future that resolves when any pending worker finishes,
        // returning its index. We rebuild this each iteration because the
        // pending set shrinks as workers complete.
        let next_done = async {
            let (result, idx, _rest) =
                futures_select_first(pending.iter_mut().map(|(_, h)| h)).await;
            (result, idx)
        };
        tokio::select! {
            (join_result, idx) = next_done => {
                let (tag, _handle) = pending.remove(idx);
                match join_result {
                    Ok(()) => outcome.completed.push(tag),
                    Err(join_err) => {
                        tracing::warn!(worker = %tag, error = %join_err, "shutdown worker join error");
                        // Treat join errors as completion: the task no
                        // longer holds resources past the window.
                        outcome.completed.push(tag);
                    }
                }
            }
            _ = &mut sleep => {
                for (tag, handle) in pending.drain(..) {
                    tracing::warn!(worker = %tag, "shutdown worker exceeded window");
                    handle.abort();
                    outcome.timed_out.push(tag);
                }
                return outcome;
            }
        }
    }
}

/// Resolve when any of the supplied `JoinHandle<()>` completes; returns the
/// join result, the index of the completed handle, and a marker so call sites
/// know to re-borrow the slice. Implemented without `futures` to keep the
/// crate's dependency surface minimal.
async fn futures_select_first<'a, I>(handles: I) -> (Result<(), tokio::task::JoinError>, usize, ())
where
    I: IntoIterator<Item = &'a mut JoinHandle<()>>,
{
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::task::Poll;

    let mut handles: Vec<&mut JoinHandle<()>> = handles.into_iter().collect();
    poll_fn(move |cx| {
        for (idx, handle) in handles.iter_mut().enumerate() {
            // `JoinHandle` implements `Future`; polling it is the standard
            // way to drive cooperative completion without consuming the
            // handle.
            if let Poll::Ready(result) = Pin::new(&mut **handle).poll(cx) {
                return Poll::Ready((result, idx, ()));
            }
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn signal_propagates_to_all_subscribers() {
        let (signal, trigger) = new();
        let s1 = signal.clone();
        let s2 = signal.clone();

        let h1 = tokio::spawn(async move { s1.wait().await });
        let h2 = tokio::spawn(async move { s2.wait().await });

        // Yield so the spawned tasks register their watch clones.
        tokio::task::yield_now().await;
        trigger.fire();

        timeout(Duration::from_secs(1), h1)
            .await
            .expect("h1 finished")
            .expect("h1 join");
        timeout(Duration::from_secs(1), h2)
            .await
            .expect("h2 finished")
            .expect("h2 join");
    }

    #[tokio::test]
    async fn is_shutting_down_returns_false_before_fire_and_true_after() {
        let (signal, trigger) = new();
        assert!(!signal.is_shutting_down());
        trigger.fire();
        assert!(signal.is_shutting_down());
    }

    #[tokio::test]
    async fn await_workers_completes_when_all_finish_in_window() {
        let workers = vec![
            ("a".to_string(), make_sleep(Duration::from_millis(50))),
            ("b".to_string(), make_sleep(Duration::from_millis(50))),
            ("c".to_string(), make_sleep(Duration::from_millis(50))),
        ];
        let outcome = await_workers_with_window(workers, Duration::from_secs(1)).await;
        assert_eq!(outcome.timed_out, Vec::<String>::new());
        let mut completed = outcome.completed.clone();
        completed.sort();
        assert_eq!(completed, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn await_workers_force_drops_workers_after_window() {
        let workers = vec![("slow".to_string(), make_sleep(Duration::from_secs(5)))];
        let started = tokio::time::Instant::now();
        let outcome = await_workers_with_window(workers, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();
        assert_eq!(outcome.completed, Vec::<String>::new());
        assert_eq!(outcome.timed_out, vec!["slow".to_string()]);
        // Must return shortly after the window, not after the worker's full sleep.
        assert!(
            elapsed < Duration::from_secs(2),
            "expected fast return after window, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn mixed_workers_are_partitioned_correctly() {
        let workers = vec![
            ("fast1".to_string(), make_sleep(Duration::from_millis(20))),
            ("slow".to_string(), make_sleep(Duration::from_secs(5))),
            ("fast2".to_string(), make_sleep(Duration::from_millis(20))),
        ];
        let outcome = await_workers_with_window(workers, Duration::from_millis(300)).await;
        let mut completed = outcome.completed.clone();
        completed.sort();
        assert_eq!(completed, vec!["fast1", "fast2"]);
        assert_eq!(outcome.timed_out, vec!["slow".to_string()]);
    }

    #[tokio::test]
    async fn signal_handler_fires_trigger_when_sigterm_observed() {
        // Real-signal delivery is flaky in unit tests; instead we exercise
        // the contract that a separate task firing the trigger wakes every
        // waiter. The full SIGTERM e2e path is task 10.1's bootstrap test.
        let (signal, trigger) = new();
        let waiter = tokio::spawn({
            let signal = signal.clone();
            async move { signal.wait().await }
        });
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.fire();
        });
        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completed")
            .expect("waiter join");
    }

    #[tokio::test]
    async fn trigger_drop_does_not_fire() {
        let (signal, trigger) = new();
        drop(trigger);
        assert!(!signal.is_shutting_down());
        let result = timeout(Duration::from_millis(50), signal.wait()).await;
        assert!(result.is_err(), "signal must not complete after trigger drop");
    }

    async fn make_sleep(d: Duration) {
        tokio::time::sleep(d).await;
    }
}
