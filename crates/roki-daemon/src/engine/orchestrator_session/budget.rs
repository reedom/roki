//! Orchestrator session budget, stall detection, and orchestrator-dead
//! routing helpers.
//!
//! Three small surfaces consumed by the orchestrator core (task 7.x):
//! 1. [`OrchestratorBudget`] — counts `run_phase` slots so the daemon can
//!    refuse to spawn another phase past `extension.orchestrator.max_phases`.
//!    Daemon-internal phase replay (Req 4.7) consumes zero slots, exposed as
//!    [`OrchestratorBudget::consume_replay`] for clarity at call sites.
//! 2. [`StallWatcher` ] — last-stdout-activity clock. The orchestrator core
//!    feeds stdout activity in via [`StallWatcher::record_activity`] and
//!    awaits [`StallWatcher::wait_for_stall`] which resolves either when the
//!    configured `extension.orchestrator.stall_seconds` window elapses
//!    without activity or when shutdown fires.
//! 3. [`route_orchestrator_dead_reason`] — pure mapping from a documented
//!    `OrchestratorDeadEvent` cause to the canonical [`InactiveReason`] the
//!    state machine consumes when it transitions
//!    `Pending -> Inactive(...)` via `OrchestratorDead`.
//!
//! Spec refs: requirements.md Req 4.7, 5.3, 5.4, 5.5; design.md
//! "Per-issue ticket lifecycle" + "Orchestrator session budget".

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::orchestrator::state::InactiveReason;
use crate::shutdown::ShutdownSignal;

/// Per-issue orchestrator budget. Counts only operator-visible `run_phase`
/// nominations; daemon-internal replay is metered at zero so a long-lived
/// session does not erode the operator-configured bound on real phases.
#[derive(Debug, Clone, Copy)]
pub struct OrchestratorBudget {
    max_phases: u32,
    used: u32,
}

/// Result of attempting to consume one budget slot. The caller surfaces
/// `Exhausted` as `Inactive(orchestrator_budget_exhausted)` and refuses the
/// additional phase spawn (Req 5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeResult {
    Ok { remaining: u32 },
    Exhausted { remaining: u32 },
}

impl OrchestratorBudget {
    /// Build a fresh budget bounded at `max_phases`.
    pub fn new(max_phases: u32) -> Self {
        Self {
            max_phases,
            used: 0,
        }
    }

    /// Configured upper bound.
    pub fn max_phases(&self) -> u32 {
        self.max_phases
    }

    /// Number of slots already consumed.
    pub fn used(&self) -> u32 {
        self.used
    }

    /// Remaining slots before exhaustion.
    pub fn remaining(&self) -> u32 {
        self.max_phases.saturating_sub(self.used)
    }

    /// Consume one slot for an operator-visible `run_phase` nomination.
    pub fn try_consume(&mut self) -> ConsumeResult {
        if self.used >= self.max_phases {
            return ConsumeResult::Exhausted { remaining: 0 };
        }
        self.used += 1;
        ConsumeResult::Ok {
            remaining: self.max_phases - self.used,
        }
    }

    /// Daemon-internal phase replay consumes zero budget. Provided as an
    /// explicit no-op so callers can document intent at the call site rather
    /// than silently skipping the consume call (Req 4.7).
    pub fn consume_replay(&self) {
        // Intentional no-op: replay never erodes the operator-visible budget.
    }
}

/// Outcome of awaiting the stall window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallOutcome {
    /// `stall_seconds` elapsed without any `record_activity` call.
    Stalled,
    /// Daemon shutdown fired before the stall window elapsed.
    Shutdown,
}

/// Watches for orchestrator stdout silence longer than `stall_seconds`.
///
/// Construction takes the configured window; the orchestrator core calls
/// [`StallWatcher::record_activity`] on every stdout line it observes. A
/// dedicated task owns the [`StallWatcher::wait_for_stall`] future and races
/// it against the daemon-wide shutdown signal.
#[derive(Debug)]
pub struct StallWatcher {
    stall_seconds: Duration,
    last_stdout_at: Mutex<Instant>,
}

impl StallWatcher {
    /// Build a fresh watcher; the activity clock starts now so a freshly
    /// spawned orchestrator gets the full first window.
    pub fn new(stall_seconds: Duration) -> Self {
        Self {
            stall_seconds,
            last_stdout_at: Mutex::new(Instant::now()),
        }
    }

    /// Configured stall window.
    pub fn stall_seconds(&self) -> Duration {
        self.stall_seconds
    }

    /// Mark a fresh stdout line. Cheap; safe from any task.
    pub fn record_activity(&self) {
        if let Ok(mut guard) = self.last_stdout_at.lock() {
            *guard = Instant::now();
        }
    }

    /// Resolve when either:
    /// - `stall_seconds` has elapsed since the most recent
    ///   `record_activity` call (returns [`StallOutcome::Stalled`]); or
    /// - `shutdown` fires first (returns [`StallOutcome::Shutdown`]).
    ///
    /// The watcher polls on a tokio `sleep_until` keyed off the last activity
    /// timestamp so concurrent `record_activity` updates extend the window
    /// rather than racing the timer.
    pub async fn wait_for_stall(&self, shutdown: ShutdownSignal) -> StallOutcome {
        loop {
            let last = match self.last_stdout_at.lock() {
                Ok(guard) => *guard,
                Err(_) => return StallOutcome::Stalled,
            };
            let deadline = last + self.stall_seconds;
            let now = Instant::now();
            if now >= deadline {
                return StallOutcome::Stalled;
            }
            let remaining = deadline - now;
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {
                    // Re-check: activity may have landed during the sleep,
                    // pushing the deadline forward.
                    let latest = match self.last_stdout_at.lock() {
                        Ok(guard) => *guard,
                        Err(_) => return StallOutcome::Stalled,
                    };
                    if latest <= last {
                        return StallOutcome::Stalled;
                    }
                    // Activity bumped the clock; loop and recompute the
                    // deadline against the new `latest`.
                }
                _ = shutdown.wait() => {
                    return StallOutcome::Shutdown;
                }
            }
        }
    }
}

/// Documented causes of orchestrator process death the daemon recognizes
/// when routing the synthesized `Pending -> Inactive(...)` transition via
/// the `OrchestratorDead` trigger (Req 5.3, 5.4, 5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestratorDeadEvent {
    /// Non-zero exit code, signal-killed exit, or graceful exit without an
    /// `action=stop` terminal turn.
    NonCleanExit {
        exit_label: String,
    },
    /// Two consecutive turns hit schema drift; caller captures the raw
    /// stdout in a structured log alongside the inactive transition.
    SecondConsecutiveDrift,
    /// `extension.orchestrator.max_phases` exhausted without a terminal stop.
    BudgetExhausted,
}

/// Map an [`OrchestratorDeadEvent`] to its canonical [`InactiveReason`].
pub fn route_orchestrator_dead_reason(event: OrchestratorDeadEvent) -> InactiveReason {
    match event {
        OrchestratorDeadEvent::NonCleanExit { .. } => InactiveReason::OrchestratorCrash,
        OrchestratorDeadEvent::SecondConsecutiveDrift => InactiveReason::OrchestratorUnparseable,
        OrchestratorDeadEvent::BudgetExhausted => InactiveReason::OrchestratorBudgetExhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_consumes_until_exhausted_then_refuses_more() {
        let mut budget = OrchestratorBudget::new(2);
        assert_eq!(budget.remaining(), 2);

        match budget.try_consume() {
            ConsumeResult::Ok { remaining } => assert_eq!(remaining, 1),
            other => panic!("expected Ok(remaining=1), got {other:?}"),
        }
        match budget.try_consume() {
            ConsumeResult::Ok { remaining } => assert_eq!(remaining, 0),
            other => panic!("expected Ok(remaining=0), got {other:?}"),
        }
        match budget.try_consume() {
            ConsumeResult::Exhausted { remaining } => assert_eq!(remaining, 0),
            other => panic!("expected Exhausted, got {other:?}"),
        }
        // Subsequent attempts stay Exhausted; the budget is not reset.
        assert!(matches!(
            budget.try_consume(),
            ConsumeResult::Exhausted { .. }
        ));
    }

    #[test]
    fn replay_consumes_zero_slots() {
        let budget = OrchestratorBudget::new(2);
        budget.consume_replay();
        budget.consume_replay();
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.remaining(), 2);
    }

    #[test]
    fn budget_zero_max_immediately_exhausted() {
        let mut budget = OrchestratorBudget::new(0);
        assert!(matches!(
            budget.try_consume(),
            ConsumeResult::Exhausted { remaining: 0 }
        ));
    }

    #[tokio::test]
    async fn stall_watcher_resolves_stalled_after_window() {
        // Real time with a tight window keeps the test fast and avoids
        // pulling in tokio's `test-util` feature (no new dep budget).
        let watcher = StallWatcher::new(Duration::from_millis(80));
        let (signal, _trigger) = crate::shutdown::new();

        let started = Instant::now();
        let outcome = watcher.wait_for_stall(signal).await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, StallOutcome::Stalled);
        // Lower bound: ~the configured window. Upper bound: generous to
        // tolerate scheduler jitter on busy CI.
        assert!(
            elapsed >= Duration::from_millis(70),
            "stalled too early ({elapsed:?}), expected ≥ window"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "stalled too late ({elapsed:?})"
        );
    }

    #[tokio::test]
    async fn stall_watcher_record_activity_extends_window() {
        let watcher = StallWatcher::new(Duration::from_millis(120));
        let (signal, _trigger) = crate::shutdown::new();

        // Bump the clock partway through so the watcher must wait the full
        // window from the bumped timestamp rather than the original.
        tokio::time::sleep(Duration::from_millis(60)).await;
        watcher.record_activity();
        let resumed_at = Instant::now();

        let outcome = watcher.wait_for_stall(signal).await;
        let elapsed_since_bump = resumed_at.elapsed();
        assert_eq!(outcome, StallOutcome::Stalled);
        assert!(
            elapsed_since_bump >= Duration::from_millis(110),
            "stall fired before extended window elapsed ({elapsed_since_bump:?})"
        );
    }

    #[tokio::test]
    async fn stall_watcher_resolves_shutdown_when_signal_fires_first() {
        let watcher = StallWatcher::new(Duration::from_secs(60));
        let (signal, trigger) = crate::shutdown::new();

        let waiter = tokio::spawn(async move { watcher.wait_for_stall(signal).await });

        // Yield once so the waiter parks on the select.
        tokio::task::yield_now().await;
        trigger.fire();

        let outcome = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completed")
            .expect("waiter join");
        assert_eq!(outcome, StallOutcome::Shutdown);
    }

    #[test]
    fn route_dead_reason_maps_each_documented_cause() {
        assert_eq!(
            route_orchestrator_dead_reason(OrchestratorDeadEvent::NonCleanExit {
                exit_label: "exit code 137 (SIGKILL)".to_owned(),
            }),
            InactiveReason::OrchestratorCrash
        );
        assert_eq!(
            route_orchestrator_dead_reason(OrchestratorDeadEvent::SecondConsecutiveDrift),
            InactiveReason::OrchestratorUnparseable
        );
        assert_eq!(
            route_orchestrator_dead_reason(OrchestratorDeadEvent::BudgetExhausted),
            InactiveReason::OrchestratorBudgetExhausted
        );
    }
}
