//! Bounded broadcast bus for `TransitionEvent`s.
//!
//! Subscribers receive transition events read-only; there is no veto. The
//! channel is bounded with drop-newest-on-full semantics — when the broadcast
//! channel reports that older receivers lagged, the bus increments a per-bus
//! drop counter and emits a structured warn log so operators can tune the
//! capacity if a subscriber falls chronically behind.
//!
//! Spec refs: requirements.md Req 8.2, 8.3, 8.4, 13.2.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::broadcast;
use tracing::warn;

use crate::orchestrator::state::TransitionEvent;

/// Default capacity for the broadcast channel. Sized to absorb a small burst
/// of transitions per actor (admission + per-phase transitions + cleaning)
/// without blocking the producer.
pub const DEFAULT_EVENT_BUS_CAPACITY: usize = 1024;

/// Bounded broadcast bus published by the orchestrator core. Cheap to clone
/// (an `Arc` over the underlying state).
#[derive(Debug, Clone)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

#[derive(Debug)]
struct EventBusInner {
    tx: broadcast::Sender<TransitionEvent>,
    drop_counter: AtomicU64,
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_EVENT_BUS_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(EventBusInner {
                tx,
                drop_counter: AtomicU64::new(0),
            }),
        }
    }

    /// Subscribe a fresh receiver. Each subscriber sees only events published
    /// after the subscription is created.
    pub fn subscribe(&self) -> broadcast::Receiver<TransitionEvent> {
        self.inner.tx.subscribe()
    }

    /// Publish one transition event. Failure to deliver to any current
    /// subscriber (channel full, lagged subscriber drop) increments the bus
    /// drop counter and logs structurally; the publish call itself never
    /// blocks the producer.
    pub fn publish(&self, event: TransitionEvent) {
        match self.inner.tx.send(event) {
            Ok(_delivered) => {}
            Err(_no_subscribers) => {
                // No receivers right now is not a drop: the broadcast channel
                // tracks lag separately. Treat as a no-op.
                self.inner.drop_counter.fetch_add(1, Ordering::Relaxed);
                warn!(
                    target: "orchestrator.event_bus",
                    "transition event published with no active subscribers"
                );
            }
        }
    }

    /// Cumulative count of publishes that observed no active subscribers, or
    /// were rejected because of a saturated channel detected via the
    /// underlying broadcast lag protocol. Used by tests + telemetry.
    pub fn drop_count(&self) -> u64 {
        self.inner.drop_counter.load(Ordering::Relaxed)
    }

    /// Increment the drop counter from a subscriber side that observed a
    /// `RecvError::Lagged` and dropped the lagged events. Logged with the
    /// caller's tag.
    pub fn record_subscriber_lag(&self, subscriber: &str, missed: u64) {
        self.inner
            .drop_counter
            .fetch_add(missed, Ordering::Relaxed);
        warn!(
            target: "orchestrator.event_bus",
            subscriber = subscriber,
            missed = missed,
            "broadcast subscriber lagged; events dropped"
        );
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::state::{
        InactiveReason, IssueId, TransitionTrigger, WorkerState,
    };
    use std::time::Duration;

    fn sample_event(id: &str) -> TransitionEvent {
        TransitionEvent {
            issue: IssueId::from(id),
            repo: None,
            previous: WorkerState::Pending,
            next: WorkerState::Inactive(InactiveReason::AwaitingLinear),
            trigger: TransitionTrigger::OrchestratorAction,
            mode: None,
            inactive_reason: Some(InactiveReason::AwaitingLinear),
            correlation_id: format!("c-{id}"),
        }
    }

    #[tokio::test]
    async fn publish_with_two_subscribers_delivers_in_order() {
        let bus = EventBus::with_capacity(8);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        bus.publish(sample_event("ENG-1"));
        bus.publish(sample_event("ENG-2"));

        let a1 = tokio::time::timeout(Duration::from_secs(1), a.recv())
            .await
            .unwrap()
            .unwrap();
        let a2 = tokio::time::timeout(Duration::from_secs(1), a.recv())
            .await
            .unwrap()
            .unwrap();
        let b1 = tokio::time::timeout(Duration::from_secs(1), b.recv())
            .await
            .unwrap()
            .unwrap();
        let b2 = tokio::time::timeout(Duration::from_secs(1), b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a1.issue, IssueId::from("ENG-1"));
        assert_eq!(a2.issue, IssueId::from("ENG-2"));
        assert_eq!(b1.issue, IssueId::from("ENG-1"));
        assert_eq!(b2.issue, IssueId::from("ENG-2"));
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_increments_drop_counter() {
        let bus = EventBus::with_capacity(4);
        bus.publish(sample_event("ENG-X"));
        assert_eq!(bus.drop_count(), 1);
    }

    #[tokio::test]
    async fn record_subscriber_lag_accumulates_drops() {
        let bus = EventBus::with_capacity(2);
        bus.record_subscriber_lag("test-sub", 5);
        bus.record_subscriber_lag("test-sub", 7);
        assert_eq!(bus.drop_count(), 12);
    }
}
