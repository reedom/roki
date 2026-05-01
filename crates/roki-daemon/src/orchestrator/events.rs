//! Transition event bus and subscription hooks.
//!
//! This module ships the [`EventBus`] — the orchestrator-owned dispatcher
//! that publishes [`TransitionEvent`]s to registered subscribers. It is the
//! additive extension surface pinned by requirements 8.2, 8.3, and 8.4 and
//! by the design.md "EventBus, SubscriberHooks" section: a single tokio
//! broadcast channel for non-vetoable transitions plus an explicit
//! await-on-each-subscriber path for vetoable transitions where a `Deny`
//! decision blocks the transition.
//!
//! ## What ships in 3.1
//!
//! * [`SubscriberError`] — the typed error returned by trait methods.
//! * [`TransitionSubscriber`] trait — the dual-method shape from design.md
//!   (`on_transition` for observation, `veto` for vetoable transitions).
//! * [`EventBus`] — registration, broadcast publication for non-vetoable
//!   events, sequential vetoable evaluation, and per-subscriber error /
//!   drop counter bookkeeping.
//!
//! ## Two dispatch paths
//!
//! * **Non-vetoable transitions** (the broadcast path). Every registered
//!   subscriber receives the event through a `tokio::sync::broadcast`
//!   channel sized at 256 slots. Each subscriber's receive loop runs in its
//!   own spawned task, so a panicking or slow subscriber cannot stall any
//!   other subscriber. If the channel lags a subscriber, the receiver gets
//!   `RecvError::Lagged(n)`, the bus increments that subscriber's drop
//!   counter, and emits a `subscriber.drop` log line tagged with the
//!   subscriber identifier.
//! * **Vetoable transitions** (the explicit path). The bus walks the
//!   registered subscribers in registration order and awaits each one's
//!   `veto` method sequentially. The aggregation rule is strict
//!   `any-Deny-blocks`: any returned `Deny` (or any error, which is
//!   treated as `Deny` to fail closed per design.md) causes the aggregated
//!   decision to be `Deny`; remaining subscribers are still consulted so
//!   each gets to observe the vetoable transition. The first `Deny` reason
//!   is the one carried in the aggregated decision.
//!
//! ## Error isolation
//!
//! * Observer errors are recorded in a per-subscriber `AtomicU64` error
//!   counter and logged with the subscriber identifier. They never affect
//!   other subscribers.
//! * Subscriber panics in the broadcast path are caught at the task
//!   boundary by tokio's `JoinHandle::is_panic` semantics — the spawned
//!   receive loop wraps the user-provided `on_transition` in
//!   `tokio::spawn` per event so a panic terminates only that single
//!   delivery, increments the error counter, and the receive loop continues
//!   with the next event.
//! * A `Deny` for a non-vetoable transition is a programmer error; the bus
//!   logs and ignores it (it never asks observers for a veto on those
//!   transitions in the first place).
//!
//! ## What this module does NOT do
//!
//! * It does not own or drive the state machine. The orchestrator core
//!   (task 3.2) owns the state and calls into [`EventBus::publish`] on
//!   every committed transition.
//! * It does not expose a deregistration handle yet. The MVP registry is
//!   append-only; subscription handles will be wired in by the orchestrator
//!   core that owns lifecycle.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{error, warn};

use super::state::{TransitionEvent, VetoDecision};

/// Default capacity of the non-vetoable broadcast channel.
///
/// Sized to comfortably absorb a burst of transitions across all active
/// `(repo, issue)` workers without back-pressuring publication. A
/// subscriber that lags past this many events between recv calls will see
/// `RecvError::Lagged` and trigger the documented drop-counter increment.
const DEFAULT_BROADCAST_CAPACITY: usize = 256;

/// Typed error returned by [`TransitionSubscriber`] methods.
///
/// Subscribers carry their own internal error types; converting them into
/// [`SubscriberError::Other`] at the trait boundary keeps the bus
/// independent of subscriber-specific error enums while preserving the
/// human-readable detail for structured logs.
#[derive(Debug, Error)]
pub enum SubscriberError {
    /// Catch-all variant carrying a human-readable message.
    #[error("{0}")]
    Other(String),
}

impl SubscriberError {
    /// Build a `SubscriberError::Other` from any displayable value.
    pub fn other(msg: impl Into<String>) -> Self {
        SubscriberError::Other(msg.into())
    }
}

/// Observer of orchestrator transitions.
///
/// Implementors are registered with [`EventBus::register`] and dispatched
/// on every committed transition. The trait carries two methods:
///
/// * `on_transition` is invoked for every transition (broadcast path).
///   Errors are isolated and counted; a panic terminates only that single
///   delivery.
/// * `veto` is invoked only for the vetoable transition subset
///   (`Queued -> Active`, `AwaitingReview -> TerminalSuccess`,
///   `TerminalSuccess -> Cleaning`). Implementors that do not participate
///   in veto decisions can return `Ok(VetoDecision::Allow)` from
///   `veto`. Returning `Err` is treated as `Deny` to fail closed per
///   design.md.
///
/// Implementors must be `Send + Sync + 'static` so they can be held in an
/// `Arc<dyn TransitionSubscriber>` across tokio task boundaries.
#[async_trait]
pub trait TransitionSubscriber: Send + Sync + 'static {
    /// Stable identifier used in structured logs and error counters.
    fn id(&self) -> &str;

    /// Observe a committed transition. Errors are isolated and counted.
    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError>;

    /// Vote on a vetoable transition. Defaults to `Allow` for subscribers
    /// that do not participate in veto decisions; an error is treated as
    /// `Deny` by the bus.
    async fn veto(&self, _event: &TransitionEvent) -> Result<VetoDecision, SubscriberError> {
        Ok(VetoDecision::Allow)
    }
}

/// Per-subscriber bookkeeping recorded by the bus.
///
/// `error_count` is bumped every time `on_transition` or `veto` returns an
/// error or panics; `drop_count` is bumped every time the broadcast
/// receiver lags and the runtime drops events for this subscriber.
#[derive(Debug, Default)]
struct SubscriberCounters {
    error_count: AtomicU64,
    drop_count: AtomicU64,
}

/// Snapshot of a subscriber's counters at a point in time. Useful for
/// integration tests and for the observability surface that wraps the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriberMetrics {
    pub id: String,
    pub error_count: u64,
    pub drop_count: u64,
}

/// Inner registration record held by [`EventBus`].
struct Registration {
    subscriber: Arc<dyn TransitionSubscriber>,
    counters: Arc<SubscriberCounters>,
}

impl Clone for Registration {
    fn clone(&self) -> Self {
        Self {
            subscriber: Arc::clone(&self.subscriber),
            counters: Arc::clone(&self.counters),
        }
    }
}

/// Transition event bus.
///
/// One instance per orchestrator. Cheap to clone — the bus holds its
/// internal state behind `Arc`s so handles can be passed to subscriber
/// driver tasks and to the orchestrator core without extra wiring.
///
/// See the module docs for the full dispatch contract.
#[derive(Clone)]
pub struct EventBus {
    /// Broadcast sender used by the non-vetoable path. The receiver count
    /// is the number of currently subscribed observer driver tasks.
    sender: broadcast::Sender<TransitionEvent>,
    /// All registered subscribers and their counters. The mutex is held
    /// only across registration / snapshot — never across `await`.
    registrations: Arc<Mutex<Vec<Registration>>>,
}

impl EventBus {
    /// Construct a new bus with an explicit broadcast capacity. Tests use
    /// this to dial the channel small enough to assert lag-induced drops.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self {
            sender,
            registrations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Default-capacity bus.
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_BROADCAST_CAPACITY)
    }

    /// Register a subscriber for both observer and (if it participates in
    /// veto decisions) vetoable dispatch.
    ///
    /// Spawns a long-lived tokio task that drives the subscriber's
    /// `on_transition` from the broadcast receiver. The task ends when the
    /// bus is dropped (all senders gone). Errors and panics inside the
    /// subscriber are isolated to the per-event delivery: an error
    /// increments the error counter and is logged; a panic terminates only
    /// that single delivery and is logged by tokio's task machinery.
    ///
    /// Returns the count of registered subscribers after this call.
    pub fn register(&self, subscriber: Arc<dyn TransitionSubscriber>) -> usize {
        let counters = Arc::new(SubscriberCounters::default());
        let registration = Registration {
            subscriber: Arc::clone(&subscriber),
            counters: Arc::clone(&counters),
        };
        let count = {
            let mut guard = self.lock_registrations();
            guard.push(registration);
            guard.len()
        };

        // Spawn the broadcast receive loop. The receiver is created here
        // so it can only observe events published after registration —
        // which matches the documented "subscribe and start observing"
        // semantics and avoids retroactive replay.
        let receiver = self.sender.subscribe();
        spawn_observer_loop(subscriber, counters, receiver);
        count
    }

    /// Publish a transition event.
    ///
    /// * For non-vetoable events, returns immediately after the broadcast
    ///   send: subscribers receive concurrently via their own tasks.
    /// * For vetoable events, sequentially awaits each subscriber's `veto`
    ///   method, aggregating `any-Deny-blocks`, and ALSO publishes through
    ///   the broadcast path so observation-only subscribers still see the
    ///   transition.
    ///
    /// Returns the aggregated [`VetoDecision`]. Non-vetoable transitions
    /// always return `Allow`.
    pub async fn publish(&self, event: TransitionEvent) -> VetoDecision {
        // Always publish through the broadcast path so observers see the
        // event regardless of veto outcome. `send` returning `Err` means
        // there are no live receivers; that is fine — the event is simply
        // not observed by anybody.
        let _ = self.sender.send(event.clone());

        if !event.vetoable {
            return VetoDecision::Allow;
        }

        self.evaluate_vetoable(&event).await
    }

    /// Evaluate the vetoable subscriber chain for `event`.
    ///
    /// Subscribers are walked in registration order. Each subscriber's
    /// `veto` is awaited sequentially. The aggregation rule:
    ///
    /// * Empty registry → `Allow`.
    /// * Any `Err` from a subscriber is recorded and treated as
    ///   `Deny { reason: "<id>: <error>" }` (fail closed per design.md).
    /// * The FIRST `Deny` (returned or synthesized from an error) wins;
    ///   remaining subscribers are still invoked so each observes the
    ///   transition exactly once.
    async fn evaluate_vetoable(&self, event: &TransitionEvent) -> VetoDecision {
        let snapshot = self.snapshot_registrations();
        let mut aggregated = VetoDecision::Allow;
        for registration in snapshot {
            let id = registration.subscriber.id().to_string();
            let result = registration.subscriber.veto(event).await;
            let decision = match result {
                Ok(decision) => decision,
                Err(err) => {
                    registration
                        .counters
                        .error_count
                        .fetch_add(1, Ordering::SeqCst);
                    error!(
                        subscriber = %id,
                        error = %err,
                        "subscriber.veto.error: treating as Deny (fail-closed)",
                    );
                    VetoDecision::deny(format!("{id}: {err}"))
                }
            };
            if aggregated.is_allow() {
                aggregated = decision;
            }
        }
        aggregated
    }

    /// Snapshot the per-subscriber counters. Useful for tests and the
    /// observability surface; never used on the publish hot path.
    pub fn metrics(&self) -> Vec<SubscriberMetrics> {
        self.snapshot_registrations()
            .into_iter()
            .map(|registration| SubscriberMetrics {
                id: registration.subscriber.id().to_string(),
                error_count: registration.counters.error_count.load(Ordering::SeqCst),
                drop_count: registration.counters.drop_count.load(Ordering::SeqCst),
            })
            .collect()
    }

    /// Number of registered subscribers.
    pub fn len(&self) -> usize {
        self.lock_registrations().len()
    }

    /// Returns `true` iff no subscribers are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock_registrations(&self) -> std::sync::MutexGuard<'_, Vec<Registration>> {
        self.registrations
            .lock()
            .expect("EventBus registrations mutex poisoned; this is unrecoverable")
    }

    fn snapshot_registrations(&self) -> Vec<Registration> {
        self.lock_registrations().clone()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

/// Spawn the long-lived broadcast receive loop for `subscriber`.
///
/// Each delivery is dispatched in a fresh `tokio::spawn` so a panic in
/// `on_transition` terminates only that single delivery, not the receive
/// loop. Errors increment the per-subscriber error counter and are logged
/// with the subscriber identifier. Lag (i.e. `RecvError::Lagged`)
/// increments the drop counter and is logged.
fn spawn_observer_loop(
    subscriber: Arc<dyn TransitionSubscriber>,
    counters: Arc<SubscriberCounters>,
    mut receiver: broadcast::Receiver<TransitionEvent>,
) {
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    dispatch_observer(&subscriber, &counters, event).await;
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    counters.drop_count.fetch_add(missed, Ordering::SeqCst);
                    warn!(
                        subscriber = %subscriber.id(),
                        missed = missed,
                        "subscriber.drop: broadcast lag, events dropped",
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Dispatch a single event delivery in its own spawned task so a panic in
/// the subscriber's `on_transition` terminates only the delivery.
async fn dispatch_observer(
    subscriber: &Arc<dyn TransitionSubscriber>,
    counters: &Arc<SubscriberCounters>,
    event: TransitionEvent,
) {
    let subscriber = Arc::clone(subscriber);
    let counters = Arc::clone(counters);
    let id = subscriber.id().to_string();

    let join = tokio::spawn(async move { subscriber.on_transition(&event).await });

    match join.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            counters.error_count.fetch_add(1, Ordering::SeqCst);
            error!(
                subscriber = %id,
                error = %err,
                "subscriber.on_transition.error",
            );
        }
        Err(join_err) if join_err.is_panic() => {
            counters.error_count.fetch_add(1, Ordering::SeqCst);
            error!(
                subscriber = %id,
                "subscriber.on_transition.panic: delivery aborted, receive loop continues",
            );
        }
        Err(join_err) => {
            // Cancellation: bus is shutting down or the event was racing
            // shutdown. Record but do not log loudly.
            counters.error_count.fetch_add(1, Ordering::SeqCst);
            warn!(
                subscriber = %id,
                error = %join_err,
                "subscriber.on_transition.cancelled",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Compute a snapshot of every subscriber's metrics keyed by id.
    /// Helper for tests; the `metrics()` method on the bus is the public
    /// surface.
    fn metrics_by_id(bus: &EventBus) -> HashMap<String, SubscriberMetrics> {
        bus.metrics()
            .into_iter()
            .map(|m| (m.id.clone(), m))
            .collect()
    }

    use crate::orchestrator::state::{
        CorrelationId, IssueId, TransitionEvent, TransitionTrigger, VetoDecision, WorkerState,
    };
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;
    use tokio::sync::Notify;
    use uuid::Uuid;

    fn sample_non_vetoable_event() -> TransitionEvent {
        // Discovered -> Queued is legal and non-vetoable.
        TransitionEvent::new(
            IssueId::new("ENG-1"),
            WorkerState::Discovered,
            WorkerState::Queued,
            TransitionTrigger::TrackerEvent,
            CorrelationId::from_uuid(Uuid::nil()),
        )
        .expect("Discovered -> Queued must be a legal transition")
    }

    fn sample_vetoable_event() -> TransitionEvent {
        // Queued -> Active is legal and vetoable.
        TransitionEvent::new(
            IssueId::new("ENG-1"),
            WorkerState::Queued,
            WorkerState::Active,
            TransitionTrigger::TrackerEvent,
            CorrelationId::from_uuid(Uuid::nil()),
        )
        .expect("Queued -> Active must be a legal transition")
    }

    /// Counting observer that records how many events it has seen.
    struct CountingObserver {
        id: &'static str,
        count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TransitionSubscriber for CountingObserver {
        fn id(&self) -> &str {
            self.id
        }

        async fn on_transition(&self, _event: &TransitionEvent) -> Result<(), SubscriberError> {
            self.count.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }

    /// Observer that always returns an error, with a count of how many
    /// times `on_transition` was invoked.
    struct ErroringObserver {
        id: &'static str,
        invocations: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TransitionSubscriber for ErroringObserver {
        fn id(&self) -> &str {
            self.id
        }

        async fn on_transition(&self, _event: &TransitionEvent) -> Result<(), SubscriberError> {
            self.invocations.fetch_add(1, AtomicOrdering::SeqCst);
            Err(SubscriberError::other("intentional failure"))
        }
    }

    /// Observer that panics on every delivery.
    struct PanickingObserver {
        id: &'static str,
        invocations: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TransitionSubscriber for PanickingObserver {
        fn id(&self) -> &str {
            self.id
        }

        async fn on_transition(&self, _event: &TransitionEvent) -> Result<(), SubscriberError> {
            self.invocations.fetch_add(1, AtomicOrdering::SeqCst);
            panic!("intentional panic from {}", self.id);
        }
    }

    /// Vetoable subscriber that returns a fixed decision and records the
    /// number of veto invocations.
    struct FixedVetoSubscriber {
        id: &'static str,
        decision: VetoDecision,
        invocations: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TransitionSubscriber for FixedVetoSubscriber {
        fn id(&self) -> &str {
            self.id
        }

        async fn on_transition(&self, _event: &TransitionEvent) -> Result<(), SubscriberError> {
            Ok(())
        }

        async fn veto(&self, _event: &TransitionEvent) -> Result<VetoDecision, SubscriberError> {
            self.invocations.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(self.decision.clone())
        }
    }

    /// Vetoable subscriber that always errors out from `veto`.
    struct ErroringVetoSubscriber {
        id: &'static str,
        invocations: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TransitionSubscriber for ErroringVetoSubscriber {
        fn id(&self) -> &str {
            self.id
        }

        async fn on_transition(&self, _event: &TransitionEvent) -> Result<(), SubscriberError> {
            Ok(())
        }

        async fn veto(&self, _event: &TransitionEvent) -> Result<VetoDecision, SubscriberError> {
            self.invocations.fetch_add(1, AtomicOrdering::SeqCst);
            Err(SubscriberError::other("vetoable subscriber failed"))
        }
    }

    /// Wait until `condition()` is true or the timeout elapses. Used to
    /// give spawned observer driver tasks time to drain the broadcast
    /// channel without resorting to brittle fixed sleeps.
    async fn await_condition<F>(timeout: Duration, mut condition: F) -> bool
    where
        F: FnMut() -> bool,
    {
        let start = std::time::Instant::now();
        while !condition() {
            if start.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        true
    }

    #[tokio::test]
    async fn non_vetoable_publish_reaches_every_registered_observer() {
        let bus = EventBus::with_default_capacity();
        let count_a = Arc::new(AtomicUsize::new(0));
        let count_b = Arc::new(AtomicUsize::new(0));
        bus.register(Arc::new(CountingObserver {
            id: "observer-a",
            count: Arc::clone(&count_a),
        }));
        bus.register(Arc::new(CountingObserver {
            id: "observer-b",
            count: Arc::clone(&count_b),
        }));

        bus.publish(sample_non_vetoable_event()).await;

        let reached_both = await_condition(Duration::from_millis(500), || {
            count_a.load(AtomicOrdering::SeqCst) == 1 && count_b.load(AtomicOrdering::SeqCst) == 1
        })
        .await;
        assert!(
            reached_both,
            "both observers must receive the broadcast event",
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn transitions_reach_healthy_observer_when_other_errors() {
        // The observable-completion criterion from tasks.md task 3.1:
        // two subscribers, one errors on every event; transitions still
        // reach the healthy subscriber and the failure is logged with
        // the subscriber identifier.
        let bus = EventBus::with_default_capacity();
        let healthy_count = Arc::new(AtomicUsize::new(0));
        let bad_invocations = Arc::new(AtomicUsize::new(0));
        bus.register(Arc::new(CountingObserver {
            id: "healthy",
            count: Arc::clone(&healthy_count),
        }));
        bus.register(Arc::new(ErroringObserver {
            id: "always-fails",
            invocations: Arc::clone(&bad_invocations),
        }));

        // Publish three events; the failing subscriber should not stop
        // the healthy one from receiving any of them.
        for _ in 0..3 {
            bus.publish(sample_non_vetoable_event()).await;
        }

        let reached = await_condition(Duration::from_millis(500), || {
            healthy_count.load(AtomicOrdering::SeqCst) == 3
                && bad_invocations.load(AtomicOrdering::SeqCst) == 3
        })
        .await;
        assert!(
            reached,
            "healthy observer must still receive every event despite the failing subscriber",
        );

        // Per-subscriber error counter must be incremented exactly three
        // times for the failing subscriber and zero times for the healthy
        // one.
        let metrics = metrics_by_id(&bus);
        assert_eq!(
            metrics.get("always-fails").unwrap().error_count,
            3,
            "failing subscriber's error counter must reflect every failed delivery",
        );
        assert_eq!(
            metrics.get("healthy").unwrap().error_count,
            0,
            "healthy subscriber's error counter must remain at zero",
        );

        // The structured error log must name the failing subscriber so
        // operators can attribute the failure without scraping ids out of
        // payloads. tracing-test captures all events emitted on this test
        // task; assert the failing subscriber's id appears.
        assert!(
            logs_contain("subscriber.on_transition.error"),
            "the per-subscriber error must be logged with the documented event name",
        );
        assert!(
            logs_contain("always-fails"),
            "the structured log must carry the failing subscriber's identifier",
        );
    }

    #[tokio::test]
    async fn panicking_observer_does_not_kill_receive_loop() {
        let bus = EventBus::with_default_capacity();
        let healthy_count = Arc::new(AtomicUsize::new(0));
        let panic_invocations = Arc::new(AtomicUsize::new(0));
        bus.register(Arc::new(CountingObserver {
            id: "healthy",
            count: Arc::clone(&healthy_count),
        }));
        bus.register(Arc::new(PanickingObserver {
            id: "panics",
            invocations: Arc::clone(&panic_invocations),
        }));

        for _ in 0..2 {
            bus.publish(sample_non_vetoable_event()).await;
        }

        let reached = await_condition(Duration::from_millis(500), || {
            healthy_count.load(AtomicOrdering::SeqCst) == 2
                && panic_invocations.load(AtomicOrdering::SeqCst) == 2
        })
        .await;
        assert!(
            reached,
            "panicking observer must not stop subsequent deliveries",
        );

        let metrics = metrics_by_id(&bus);
        assert_eq!(
            metrics.get("panics").unwrap().error_count,
            2,
            "panic deliveries must each count as one error increment",
        );
    }

    #[tokio::test]
    async fn vetoable_subscriber_allow_passes_through() {
        let bus = EventBus::with_default_capacity();
        let invocations = Arc::new(AtomicUsize::new(0));
        bus.register(Arc::new(FixedVetoSubscriber {
            id: "spec-gate",
            decision: VetoDecision::Allow,
            invocations: Arc::clone(&invocations),
        }));

        let decision = bus.publish(sample_vetoable_event()).await;
        assert!(
            decision.is_allow(),
            "single Allow vote must aggregate to Allow"
        );
        assert_eq!(invocations.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn vetoable_subscriber_deny_blocks_transition() {
        let bus = EventBus::with_default_capacity();
        let invocations = Arc::new(AtomicUsize::new(0));
        bus.register(Arc::new(FixedVetoSubscriber {
            id: "spec-gate",
            decision: VetoDecision::deny("spec gate not green"),
            invocations: Arc::clone(&invocations),
        }));

        let decision = bus.publish(sample_vetoable_event()).await;
        match decision {
            VetoDecision::Deny { reason } => assert_eq!(reason, "spec gate not green"),
            VetoDecision::Allow => panic!("expected Deny"),
        }
        assert_eq!(invocations.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multiple_vetoable_subscribers_aggregate_to_deny_on_any_deny() {
        let bus = EventBus::with_default_capacity();
        let allow_invocations = Arc::new(AtomicUsize::new(0));
        let deny_invocations = Arc::new(AtomicUsize::new(0));
        bus.register(Arc::new(FixedVetoSubscriber {
            id: "first-allow",
            decision: VetoDecision::Allow,
            invocations: Arc::clone(&allow_invocations),
        }));
        bus.register(Arc::new(FixedVetoSubscriber {
            id: "second-deny",
            decision: VetoDecision::deny("second-deny says no"),
            invocations: Arc::clone(&deny_invocations),
        }));

        let decision = bus.publish(sample_vetoable_event()).await;
        match decision {
            VetoDecision::Deny { reason } => assert_eq!(reason, "second-deny says no"),
            VetoDecision::Allow => panic!("any Deny must dominate"),
        }
        // Every vetoable subscriber observes the transition exactly once.
        assert_eq!(allow_invocations.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(deny_invocations.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn vetoable_subscriber_error_treated_as_deny() {
        // Per design.md "EventBus, SubscriberHooks": subscriber failure on
        // a vetoable event is treated as Deny to fail closed.
        let bus = EventBus::with_default_capacity();
        let invocations = Arc::new(AtomicUsize::new(0));
        bus.register(Arc::new(ErroringVetoSubscriber {
            id: "broken-gate",
            invocations: Arc::clone(&invocations),
        }));

        let decision = bus.publish(sample_vetoable_event()).await;
        match decision {
            VetoDecision::Deny { reason } => {
                assert!(
                    reason.contains("broken-gate"),
                    "Deny reason must include subscriber id; got {reason:?}",
                );
            }
            VetoDecision::Allow => panic!("error must fail closed as Deny"),
        }
        assert_eq!(invocations.load(AtomicOrdering::SeqCst), 1);

        let metrics = metrics_by_id(&bus);
        assert_eq!(
            metrics.get("broken-gate").unwrap().error_count,
            1,
            "vetoable subscriber error must increment the per-subscriber error counter",
        );
    }

    #[tokio::test]
    async fn empty_bus_publish_is_a_noop() {
        let bus = EventBus::with_default_capacity();
        let decision = bus.publish(sample_non_vetoable_event()).await;
        assert!(decision.is_allow());
        // Vetoable publish on an empty bus also allows: nobody to deny.
        let decision = bus.publish(sample_vetoable_event()).await;
        assert!(decision.is_allow());
    }

    /// Slow observer that holds the receive loop in a single delivery
    /// until released. Used to simulate broadcast lag.
    struct GatedObserver {
        id: &'static str,
        delivered: Arc<AtomicUsize>,
        gate: Arc<Notify>,
    }

    #[async_trait]
    impl TransitionSubscriber for GatedObserver {
        fn id(&self) -> &str {
            self.id
        }

        async fn on_transition(&self, _event: &TransitionEvent) -> Result<(), SubscriberError> {
            // Wait for permission before completing the first delivery,
            // forcing subsequent events to back up in the broadcast
            // channel and eventually overflow it.
            self.gate.notified().await;
            self.delivered.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn slow_observer_lag_does_not_block_publishing() {
        // Use a small capacity so we can deterministically overflow the
        // slow subscriber's broadcast queue without publishing thousands
        // of events. The healthy subscriber's receive loop runs in its
        // own tokio task and drains as fast as the publisher pushes, so
        // a capacity sized comfortably above the burst keeps healthy
        // from incurring drops while still overflowing the gated peer.
        let bus = EventBus::new(8);
        let healthy_count = Arc::new(AtomicUsize::new(0));
        let slow_delivered = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());

        bus.register(Arc::new(CountingObserver {
            id: "healthy",
            count: Arc::clone(&healthy_count),
        }));
        bus.register(Arc::new(GatedObserver {
            id: "slow",
            delivered: Arc::clone(&slow_delivered),
            gate: Arc::clone(&gate),
        }));

        // Publish more events than the channel capacity. Publication must
        // not block — broadcast::Sender::send drops the oldest event for
        // any lagging receiver and surfaces it as RecvError::Lagged on
        // the next recv call.
        // Publish well beyond the channel capacity so the gated slow
        // subscriber's queue is guaranteed to overflow. Yielding between
        // publishes lets the healthy observer's receive loop run on the
        // shared runtime so it can keep pace; the slow observer is gated
        // and still overflows regardless.
        const PUBLISHED: usize = 32;
        for _ in 0..PUBLISHED {
            bus.publish(sample_non_vetoable_event()).await;
            tokio::task::yield_now().await;
        }

        // Healthy observer is not blocked by the slow one and sees every
        // event.
        let healthy_caught_up = await_condition(Duration::from_millis(500), || {
            healthy_count.load(AtomicOrdering::SeqCst) == PUBLISHED
        })
        .await;
        assert!(
            healthy_caught_up,
            "healthy observer must process every event regardless of slow peer",
        );

        // Release the slow observer's first delivery so its receive loop
        // can advance and surface the Lagged error path.
        gate.notify_one();

        // Wait for the slow subscriber's drop counter to register some
        // missed events. We do not assert an exact value because the
        // broadcast runtime decides exactly how many are dropped, but it
        // must be strictly positive.
        let saw_drops = await_condition(Duration::from_millis(1_000), || {
            metrics_by_id(&bus)
                .get("slow")
                .map(|m| 0 < m.drop_count)
                .unwrap_or(false)
        })
        .await;
        assert!(
            saw_drops,
            "slow subscriber must register a non-zero drop counter when broadcast lags",
        );

        // The healthy subscriber's drop counter must remain zero — drops
        // are per-subscriber, not global.
        let metrics = metrics_by_id(&bus);
        assert_eq!(
            metrics.get("healthy").unwrap().drop_count,
            0,
            "drops must not be attributed to the unaffected subscriber",
        );
    }

    #[tokio::test]
    async fn metrics_reflect_registered_subscribers() {
        let bus = EventBus::with_default_capacity();
        assert!(bus.is_empty());
        bus.register(Arc::new(CountingObserver {
            id: "first",
            count: Arc::new(AtomicUsize::new(0)),
        }));
        bus.register(Arc::new(CountingObserver {
            id: "second",
            count: Arc::new(AtomicUsize::new(0)),
        }));
        assert_eq!(bus.len(), 2);
        let ids: Vec<String> = bus.metrics().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["first".to_string(), "second".to_string()]);
    }
}
