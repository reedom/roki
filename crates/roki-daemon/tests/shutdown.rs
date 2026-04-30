//! Integration tests for bounded shutdown handling (task 1.4 / Requirement 1.3).
//!
//! These tests do not send real OS signals to the test process, since that
//! would race other tests sharing the binary. Instead, they exercise the
//! `ShutdownSignal::trigger` API that production code uses on the signal-
//! handling path. The signal-handler wiring inside `runtime::run` is exercised
//! indirectly: the `runtime::run_with_shutdown` helper used here is the same
//! shutdown-await loop, with the OS-signal future replaced by a programmatic
//! trigger.

use std::time::{Duration, Instant};

use roki_daemon::shutdown::{SHUTDOWN_WINDOW, ShutdownSignal};

/// A cooperative worker exits within the bounded shutdown window after the
/// signal is triggered.
#[tokio::test]
async fn cooperative_worker_exits_within_window() {
    let shutdown = ShutdownSignal::new();

    // Spawn a "fake long-running worker" that loops until shutdown is observed.
    let worker_shutdown = shutdown.clone();
    let worker = tokio::spawn(async move {
        // Simulate long-running work that observes the shutdown signal.
        loop {
            tokio::select! {
                _ = worker_shutdown.wait() => {
                    // Cooperative cleanup window.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    break "clean";
                }
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    });

    // Run the bounded await with a short window for testability.
    let window = Duration::from_secs(2);
    let started = Instant::now();
    shutdown.trigger();

    let outcome = roki_daemon::shutdown::await_workers_with_window(vec![worker], window).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < window + Duration::from_secs(1),
        "shutdown took longer than window + tolerance: {elapsed:?}"
    );
    assert_eq!(outcome.completed, 1, "expected one worker to complete");
    assert_eq!(outcome.timed_out, 0, "expected no workers to time out");
}

/// A worker that ignores the shutdown signal is force-aborted after the window.
#[tokio::test]
async fn uncooperative_worker_is_force_exited_after_window() {
    let shutdown = ShutdownSignal::new();

    // Spawn an uncooperative worker that never observes shutdown.
    let worker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        #[allow(unreachable_code)]
        "never"
    });

    let window = Duration::from_millis(500);
    shutdown.trigger();

    let started = Instant::now();
    let outcome = roki_daemon::shutdown::await_workers_with_window(vec![worker], window).await;
    let elapsed = started.elapsed();

    // Daemon must exit cleanly within window + small tolerance even when the
    // worker refuses to honor the signal.
    assert!(
        elapsed < window + Duration::from_secs(1),
        "force-exit took longer than window + tolerance: {elapsed:?}"
    );
    assert_eq!(outcome.timed_out, 1, "expected one worker to time out");
    assert_eq!(outcome.completed, 0, "expected zero workers to complete");
}

/// `is_shutting_down` flips after `trigger`.
#[tokio::test]
async fn shutdown_signal_observable_via_is_shutting_down() {
    let shutdown = ShutdownSignal::new();
    assert!(!shutdown.is_shutting_down());
    shutdown.trigger();
    assert!(shutdown.is_shutting_down());
}

/// Multiple subscribers observe the shutdown independently.
#[tokio::test]
async fn multiple_subscribers_each_observe_shutdown() {
    let shutdown = ShutdownSignal::new();

    let a = shutdown.clone();
    let b = shutdown.clone();

    let task_a = tokio::spawn(async move { a.wait().await });
    let task_b = tokio::spawn(async move { b.wait().await });

    shutdown.trigger();

    // Both must resolve within a short window since they share the same signal.
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        task_a.await.unwrap();
        task_b.await.unwrap();
    })
    .await;

    assert!(
        result.is_ok(),
        "subscribers did not observe shutdown in time"
    );
}

/// Sanity check: the documented shutdown window is the value adapters bind to.
/// This guards against accidental drift in the public constant.
#[test]
fn shutdown_window_default_is_documented_value() {
    assert_eq!(SHUTDOWN_WINDOW, Duration::from_secs(30));
}
