//! `TrackerRefresh` nudge trait + Linear-backed handle.
//!
//! Exposes a single non-blocking call (`nudge`) so adjacent layers (HTTP API,
//! orchestrator session) can request an out-of-cycle poll without taking on
//! the tracker's internals. The handle delegates to a `tokio::sync::watch`
//! sender that the poller (3.3) subscribes to.
//!
//! Spec refs: requirements.md Req 13.3.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::tracker::linear::BackoffState;

/// Outcome of a nudge attempt. Callers use this to decide whether to log /
/// surface a "throttled" status to the operator without retrying inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeResult {
    Accepted,
    Throttled,
    BackoffActive,
}

/// Refresh trait so the orchestrator session and HTTP layer can be tested
/// against a fake without standing up a real Linear tracker.
pub trait TrackerRefresh: Send + Sync {
    fn nudge(&self) -> NudgeResult;
}

/// Linear-backed implementation. Holds a counter sender that the poller
/// subscribes to via `watch::Receiver`; the counter increments on every
/// accepted nudge so the receiver always observes a state change.
pub struct LinearTrackerHandle {
    sender: watch::Sender<u64>,
    counter: AtomicU64,
    last_accepted: Mutex<Option<Instant>>,
    cadence_floor: Duration,
    backoff: Arc<BackoffState>,
}

impl LinearTrackerHandle {
    pub fn new(
        sender: watch::Sender<u64>,
        cadence_floor: Duration,
        backoff: Arc<BackoffState>,
    ) -> Self {
        Self {
            sender,
            counter: AtomicU64::new(0),
            last_accepted: Mutex::new(None),
            cadence_floor,
            backoff,
        }
    }

    /// Construct a handle paired with a fresh receiver suitable for the
    /// poller's `refresh_rx` slot.
    pub fn paired(
        cadence_floor: Duration,
        backoff: Arc<BackoffState>,
    ) -> (Self, watch::Receiver<u64>) {
        let (tx, rx) = watch::channel(0u64);
        (Self::new(tx, cadence_floor, backoff), rx)
    }

    fn try_accept(&self) -> NudgeResult {
        // try_lock is sufficient because nudges are cheap and contention is
        // rare; if a competing nudge holds the mutex, treat as Throttled.
        let mut guard = match self.last_accepted.try_lock() {
            Ok(g) => g,
            Err(_) => return NudgeResult::Throttled,
        };
        let now = Instant::now();
        if let Some(prev) = *guard
            && now.duration_since(prev) < self.cadence_floor
        {
            return NudgeResult::Throttled;
        }
        // Backoff check uses `try_lock` on the inner mutex; if the curve is
        // currently being updated (rare), treat as BackoffActive so callers
        // back off instead of racing.
        let next_allowed = match self.backoff_next_allowed() {
            Some(v) => v,
            None => return NudgeResult::BackoffActive,
        };
        if next_allowed > now {
            return NudgeResult::BackoffActive;
        }
        *guard = Some(now);
        let next = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        // `send` only fails when no receiver is alive; the poller holds the
        // canonical receiver, so a failure means the daemon is winding down.
        let _ = self.sender.send(next);
        NudgeResult::Accepted
    }

    fn backoff_next_allowed(&self) -> Option<Instant> {
        // Read-only peek without awaiting: if the backoff lock is held we
        // err on the side of "active". This avoids making `nudge` async.
        self.backoff.next_request_at_for_peek()
    }
}

impl TrackerRefresh for LinearTrackerHandle {
    fn nudge(&self) -> NudgeResult {
        self.try_accept()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_backoff() -> Arc<BackoffState> {
        Arc::new(BackoffState::new_for_test(
            Duration::from_millis(10),
            Duration::from_secs(60),
        ))
    }

    #[tokio::test]
    async fn first_nudge_accepted_second_throttled_within_cadence() {
        let (handle, _rx) = LinearTrackerHandle::paired(Duration::from_secs(60), fresh_backoff());
        assert_eq!(handle.nudge(), NudgeResult::Accepted);
        assert_eq!(handle.nudge(), NudgeResult::Throttled);
    }

    #[tokio::test]
    async fn nudge_after_cadence_floor_accepted_again() {
        let cadence = Duration::from_millis(40);
        let (handle, _rx) = LinearTrackerHandle::paired(cadence, fresh_backoff());
        assert_eq!(handle.nudge(), NudgeResult::Accepted);
        tokio::time::sleep(cadence + Duration::from_millis(20)).await;
        assert_eq!(handle.nudge(), NudgeResult::Accepted);
    }

    #[tokio::test]
    async fn nudge_during_backoff_returns_backoff_active() {
        let backoff = fresh_backoff();
        // Force the deadline forward so the handle observes an active curve.
        backoff.set_deadline_for_test(Instant::now() + Duration::from_secs(60));
        let (handle, _rx) =
            LinearTrackerHandle::paired(Duration::from_millis(1), backoff);
        assert_eq!(handle.nudge(), NudgeResult::BackoffActive);
    }

    #[tokio::test]
    async fn accepted_nudge_increments_counter_observable_to_receiver() {
        let (handle, mut rx) =
            LinearTrackerHandle::paired(Duration::from_millis(1), fresh_backoff());
        let initial = *rx.borrow_and_update();
        assert_eq!(handle.nudge(), NudgeResult::Accepted);
        rx.changed().await.unwrap();
        let updated = *rx.borrow_and_update();
        assert!(updated > initial);
    }
}
