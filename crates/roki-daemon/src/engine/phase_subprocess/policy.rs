//! Daemon-internal phase replay policy: ticket-level retry budget +
//! exponential backoff between phase non-clean exits.
//!
//! Per design.md "PhaseSubprocessAdapter" replay loop and Req 5.10:
//! - Replays consume ZERO `extension.orchestrator.max_phases` slots — the
//!   daemon does NOT re-deliver `phase_nonclean` to the orchestrator and
//!   does NOT request a fresh `run_phase` nomination during replay.
//! - The same [`PhaseLaunchContext`] is re-used (same `phase`, `mode`,
//!   `additional_context`, `worktree_path`, `max_turns`).
//! - On `phase_nonclean(stall)` and `phase_nonclean(max_turns_exhausted)`
//!   the loop bypasses replay and surfaces the event for the orchestrator.
//! - On replay-eligible classifications (NonZero / Signal / NonSuccessSubtype
//!   / UnknownSubtype) the loop exponentially backs off between 10s and
//!   5min until `max_attempts` is exhausted.
//! - On exhaustion: caller routes either `daemon_directive(retry_exhausted)`
//!   to the orchestrator (when alive) or `Inactive(retry_exhausted)`
//!   directly (when dead). The replay layer itself returns
//!   [`ReplayDecision::Exhausted`] without picking a side.
//!
//! Spec refs: requirements.md Req 5.7, 5.10; design.md
//! "PhaseSubprocessAdapter" replay flow.

use std::time::Duration;

use crate::engine::orchestrator_session::events::{
    NoncleanClassification, PhaseNoncleanPayload,
};
use crate::engine::phase_subprocess::catalog::PhaseLaunchContext;

/// Default ticket-level max attempts (Req 5.10: range 1..=10).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Default backoff floor (Req 5.10).
pub const DEFAULT_BACKOFF_FLOOR: Duration = Duration::from_secs(10);
/// Default backoff ceiling (Req 5.10).
pub const DEFAULT_BACKOFF_CEILING: Duration = Duration::from_secs(5 * 60);

/// Bounded retry policy applied per phase nomination. Operator-overridable
/// via `extension.phase.<name>.max_attempts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_floor: Duration,
    pub backoff_ceiling: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff_floor: DEFAULT_BACKOFF_FLOOR,
            backoff_ceiling: DEFAULT_BACKOFF_CEILING,
        }
    }
}

impl RetryPolicy {
    /// Construct a retry policy clamping `max_attempts` to the documented
    /// `1..=10` range. Backoff bounds are passed through unchanged.
    pub fn new(max_attempts: u32, backoff_floor: Duration, backoff_ceiling: Duration) -> Self {
        let max_attempts = max_attempts.clamp(1, 10);
        Self {
            max_attempts,
            backoff_floor,
            backoff_ceiling,
        }
    }
}

/// Stateful per-ticket replay loop. One instance per phase nomination; the
/// caller drops the loop on `phase_complete` and creates a fresh one on the
/// orchestrator's next nomination.
#[derive(Debug, Clone)]
pub struct ReplayLoop<'a> {
    policy: &'a RetryPolicy,
    /// Number of replays already initiated (zero before the first one).
    attempt: u32,
    /// Most recently scheduled backoff window. `None` before the first
    /// replay; updated by [`ReplayLoop::next_attempt`] each call so the
    /// curve compounds across iterations.
    last_backoff: Option<Duration>,
}

impl<'a> ReplayLoop<'a> {
    pub fn new(policy: &'a RetryPolicy) -> Self {
        Self {
            policy,
            attempt: 0,
            last_backoff: None,
        }
    }

    /// Number of replays already issued.
    pub fn attempts_used(&self) -> u32 {
        self.attempt
    }

    /// Decide what to do after a `phase_nonclean` exit. Owns the budget
    /// counter and the exponential-backoff curve.
    ///
    /// Stall and max-turns-exhausted classifications bypass replay
    /// regardless of remaining budget per Req 5.7 / 5.10.
    pub fn next_attempt(
        &mut self,
        last: PhaseNoncleanPayload,
        context: &PhaseLaunchContext,
    ) -> ReplayDecision {
        match last.classification {
            NoncleanClassification::Stall | NoncleanClassification::MaxTurnsExhausted => {
                ReplayDecision::Bypass { event: last }
            }
            NoncleanClassification::NonZero
            | NoncleanClassification::Signal
            | NoncleanClassification::NonSuccessSubtype
            | NoncleanClassification::UnknownSubtype => {
                if self.attempt + 1 >= self.policy.max_attempts {
                    ReplayDecision::Exhausted
                } else {
                    self.attempt += 1;
                    let after = self.compute_backoff();
                    self.last_backoff = Some(after);
                    ReplayDecision::Replay {
                        context: context.clone(),
                        after,
                    }
                }
            }
        }
    }

    fn compute_backoff(&self) -> Duration {
        // Standard exponential curve: floor on the first attempt, doubling
        // each subsequent attempt, clamped to the ceiling. The curve is
        // bounded between `backoff_floor` and `backoff_ceiling` per Req 5.10.
        let next = match self.last_backoff {
            None => self.policy.backoff_floor,
            Some(prev) => prev.saturating_mul(2),
        };
        if next > self.policy.backoff_ceiling {
            self.policy.backoff_ceiling
        } else if next < self.policy.backoff_floor {
            self.policy.backoff_floor
        } else {
            next
        }
    }
}

/// Decision returned by [`ReplayLoop::next_attempt`].
///
/// `Replay` carries a [`PhaseLaunchContext`], which is intentionally NOT
/// `PartialEq` (it embeds an `Arc<WorkflowPolicy>` whose equality semantics
/// are not stable). Test code asserts replay re-uses the SAME context via
/// pointer equality on the workflow handle plus field-by-field comparison.
#[derive(Debug, Clone)]
pub enum ReplayDecision {
    /// Re-spawn the same phase with the original [`PhaseLaunchContext`]
    /// after waiting `after`. Caller transitions `Active -> Backoff` for
    /// the duration, then `Backoff -> Active` and re-spawns. Replay
    /// consumes ZERO orchestrator budget slots.
    Replay {
        context: PhaseLaunchContext,
        after: Duration,
    },
    /// Stall / max-turns-exhausted: bypass replay and surface the event
    /// to the orchestrator (or route directly to `Inactive(reason)` when
    /// the orchestrator is dead — caller's responsibility).
    Bypass { event: PhaseNoncleanPayload },
    /// Replay budget exhausted. Caller emits
    /// `daemon_directive(retry_exhausted)` to the orchestrator if alive,
    /// else routes to `Inactive(retry_exhausted)` directly.
    Exhausted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::orchestrator_session::budget::OrchestratorBudget;
    use crate::engine::phase_subprocess::catalog::{PhaseLaunchContext, PhaseName};
    use crate::orchestrator::state::{IssueId, Mode};
    use crate::permissions::PermissionStrategy;
    use crate::workflow::schema::{OrchestratorConfig, WorkflowPolicy};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn empty_policy() -> Arc<WorkflowPolicy> {
        Arc::new(WorkflowPolicy {
            orchestrator: OrchestratorConfig::default(),
            phases: BTreeMap::new(),
            server: serde_json::Value::Object(Default::default()),
            blocks: BTreeMap::new(),
            raw_unknowns: serde_json::Value::Object(Default::default()),
        })
    }

    fn ctx() -> PhaseLaunchContext {
        PhaseLaunchContext {
            issue: IssueId::from("ENG-7"),
            phase: PhaseName::Implement,
            mode: Mode::SpecDriven,
            additional_context: Some("ctx-A".to_owned()),
            worktree_path: Some(PathBuf::from("/wt/eng-7")),
            session_tempdir: PathBuf::from("/tmp/session"),
            max_turns: 50,
            workflow_policy: empty_policy(),
            permission_strategy: PermissionStrategy::SettingsAllowlist {
                settings_path: PathBuf::from("/tmp/settings.json"),
            },
            allowed_tools: vec!["Read".to_owned()],
        }
    }

    fn nonclean(classification: NoncleanClassification) -> PhaseNoncleanPayload {
        PhaseNoncleanPayload {
            phase: PhaseName::Implement,
            classification,
            raw_subtype: None,
            additional_context: None,
        }
    }

    fn ctx_eq(a: &PhaseLaunchContext, b: &PhaseLaunchContext) -> bool {
        // PhaseLaunchContext lacks PartialEq; compare field-by-field to
        // assert the replay re-uses the SAME launch context.
        a.issue == b.issue
            && a.phase == b.phase
            && a.mode == b.mode
            && a.additional_context == b.additional_context
            && a.worktree_path == b.worktree_path
            && a.session_tempdir == b.session_tempdir
            && a.max_turns == b.max_turns
            && Arc::ptr_eq(&a.workflow_policy, &b.workflow_policy)
            && a.permission_strategy == b.permission_strategy
            && a.allowed_tools == b.allowed_tools
    }

    #[test]
    fn nonzero_replays_same_context_then_exhausts_at_budget() {
        let policy = RetryPolicy::new(2, Duration::from_millis(50), Duration::from_secs(60));
        let mut loop_ = ReplayLoop::new(&policy);
        let original = ctx();

        // First non-clean: must replay with the same launch context.
        let decision =
            loop_.next_attempt(nonclean(NoncleanClassification::NonZero), &original);
        match decision {
            ReplayDecision::Replay { context, after } => {
                assert_eq!(after, Duration::from_millis(50));
                assert!(
                    ctx_eq(&context, &original),
                    "replay must re-use the SAME PhaseLaunchContext",
                );
            }
            other => panic!("expected Replay, got {other:?}"),
        }

        // Second non-clean: budget already at the limit (max_attempts=2).
        let decision =
            loop_.next_attempt(nonclean(NoncleanClassification::NonZero), &original);
        assert!(matches!(decision, ReplayDecision::Exhausted));
    }

    #[test]
    fn replay_does_not_consume_orchestrator_budget() {
        // Budget starts at 0 used; calling consume_replay through the loop
        // path (via the caller helper) must NOT consume slots.
        let budget = OrchestratorBudget::new(5);
        // The loop itself does not interact with the budget; the contract
        // is that replay paths call OrchestratorBudget::consume_replay
        // (no-op). Assert the counter stays put after multiple replays.
        budget.consume_replay();
        budget.consume_replay();
        budget.consume_replay();
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.remaining(), 5);
    }

    #[test]
    fn backoff_curve_doubles_until_ceiling() {
        let policy = RetryPolicy::new(
            10,
            Duration::from_millis(50),
            Duration::from_millis(400),
        );
        let mut loop_ = ReplayLoop::new(&policy);
        let original = ctx();

        let mut afters: Vec<Duration> = Vec::new();
        for _ in 0..6 {
            let decision = loop_.next_attempt(
                nonclean(NoncleanClassification::NonZero),
                &original,
            );
            match decision {
                ReplayDecision::Replay { after, .. } => afters.push(after),
                ReplayDecision::Exhausted => break,
                other => panic!("unexpected decision: {other:?}"),
            }
        }

        // Floor → 2x → 4x → 8x → ceiling → ceiling.
        assert_eq!(afters[0], Duration::from_millis(50));
        assert_eq!(afters[1], Duration::from_millis(100));
        assert_eq!(afters[2], Duration::from_millis(200));
        assert_eq!(afters[3], Duration::from_millis(400));
        // Ceiling is sticky once reached.
        for after in afters.iter().skip(4) {
            assert_eq!(*after, Duration::from_millis(400));
        }
    }

    #[test]
    fn stall_classification_bypasses_replay_regardless_of_budget() {
        let policy = RetryPolicy::new(10, Duration::from_millis(50), Duration::from_secs(60));
        let mut loop_ = ReplayLoop::new(&policy);
        let original = ctx();
        let decision = loop_.next_attempt(nonclean(NoncleanClassification::Stall), &original);
        match decision {
            ReplayDecision::Bypass { event } => {
                assert_eq!(event.classification, NoncleanClassification::Stall);
            }
            other => panic!("expected Bypass, got {other:?}"),
        }
        // Bypass must NOT consume an attempt slot.
        assert_eq!(loop_.attempts_used(), 0);
    }

    #[test]
    fn max_turns_exhausted_classification_bypasses_replay() {
        let policy = RetryPolicy::new(10, Duration::from_millis(50), Duration::from_secs(60));
        let mut loop_ = ReplayLoop::new(&policy);
        let original = ctx();
        let decision = loop_.next_attempt(
            nonclean(NoncleanClassification::MaxTurnsExhausted),
            &original,
        );
        match decision {
            ReplayDecision::Bypass { event } => {
                assert_eq!(
                    event.classification,
                    NoncleanClassification::MaxTurnsExhausted
                );
            }
            other => panic!("expected Bypass, got {other:?}"),
        }
        assert_eq!(loop_.attempts_used(), 0);
    }

    #[test]
    fn unknown_subtype_is_retried_until_budget_exhausted() {
        let policy = RetryPolicy::new(2, Duration::from_millis(50), Duration::from_secs(60));
        let mut loop_ = ReplayLoop::new(&policy);
        let original = ctx();

        let first = loop_.next_attempt(
            nonclean(NoncleanClassification::UnknownSubtype),
            &original,
        );
        assert!(matches!(first, ReplayDecision::Replay { .. }));

        let second = loop_.next_attempt(
            nonclean(NoncleanClassification::UnknownSubtype),
            &original,
        );
        assert!(matches!(second, ReplayDecision::Exhausted));
    }

    #[test]
    fn signal_classification_is_retried_like_nonzero() {
        let policy = RetryPolicy::new(3, Duration::from_millis(50), Duration::from_secs(60));
        let mut loop_ = ReplayLoop::new(&policy);
        let original = ctx();
        let first = loop_.next_attempt(nonclean(NoncleanClassification::Signal), &original);
        assert!(matches!(first, ReplayDecision::Replay { .. }));
    }

    #[test]
    fn non_success_subtype_classification_is_retried() {
        let policy = RetryPolicy::new(3, Duration::from_millis(50), Duration::from_secs(60));
        let mut loop_ = ReplayLoop::new(&policy);
        let original = ctx();
        let first = loop_.next_attempt(
            nonclean(NoncleanClassification::NonSuccessSubtype),
            &original,
        );
        assert!(matches!(first, ReplayDecision::Replay { .. }));
    }

    #[test]
    fn retry_policy_clamps_max_attempts_to_documented_range() {
        // Below the floor → 1.
        let p = RetryPolicy::new(0, Duration::from_secs(1), Duration::from_secs(10));
        assert_eq!(p.max_attempts, 1);
        // Above the ceiling → 10.
        let p = RetryPolicy::new(99, Duration::from_secs(1), Duration::from_secs(10));
        assert_eq!(p.max_attempts, 10);
    }

    #[test]
    fn default_retry_policy_matches_documented_defaults() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.backoff_floor, Duration::from_secs(10));
        assert_eq!(p.backoff_ceiling, Duration::from_secs(5 * 60));
    }

    #[test]
    fn first_replay_uses_floor_when_last_backoff_is_none() {
        let policy =
            RetryPolicy::new(5, Duration::from_secs(7), Duration::from_secs(60));
        let mut loop_ = ReplayLoop::new(&policy);
        let original = ctx();
        match loop_.next_attempt(nonclean(NoncleanClassification::NonZero), &original) {
            ReplayDecision::Replay { after, .. } => {
                assert_eq!(after, Duration::from_secs(7));
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }
}
