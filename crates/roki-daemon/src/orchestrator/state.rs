//! Per-issue state machine types: `WorkerState`, `InactiveReason`, `Mode`,
//! `TransitionTrigger`, `TransitionEvent`, and the legal-transition validator.
//!
//! Spec refs:
//! - design.md "Per-issue ticket lifecycle" (lines 332-362) — 5-state diagram,
//!   12-value `InactiveReason`, no vetoable transitions.
//! - requirements.md Req 2.6 (mode flag), 8.1 (state machine shape), 8.2
//!   (transition events), 13.2 (extension surface stability).

use std::fmt;

use thiserror::Error;

/// Five canonical worker states. `Inactive` carries a discriminator so the
/// operator-facing reason survives across observers without a side table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkerState {
    Pending,
    Active,
    Backoff,
    Inactive(InactiveReason),
    Cleaning,
}

/// 12 documented `Inactive.reason` values. The set is closed: adding a new
/// reason is an extension-surface change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InactiveReason {
    /// Orchestrator emitted `action=stop outcome=success`; awaiting operator
    /// to close the Linear ticket. Only auto-cleanup-eligible reason.
    AwaitingLinear,
    /// Orchestrator emitted `action=stop outcome=needs_operator` pre-phase.
    NeedsOperator,
    /// Orchestrator emitted `action=stop outcome=spec_incomplete` pre-phase.
    SpecIncomplete,
    /// Orchestrator emitted `action=stop outcome=needs_split` pre-phase.
    NeedsSplit,
    /// Orchestrator emitted `action=stop outcome=allowlist_rejected` pre-phase.
    AllowlistRejected,
    /// Orchestrator process crashed (non-zero / signal) before terminal action.
    OrchestratorCrash,
    /// Two consecutive schema-drift parses on the orchestrator's stdout.
    OrchestratorUnparseable,
    /// `extension.orchestrator.max_phases` exhausted without terminal `stop`.
    OrchestratorBudgetExhausted,
    /// Phase subprocess stalled and the orchestrator was dead at detection.
    Stall,
    /// Ticket-level retry budget exhausted with no live orchestrator.
    RetryExhausted,
    /// Filesystem poison detected (worktree unusable) with no live orchestrator.
    FsPoison,
    /// Recovery scan found a worktree with no matching Linear assignment.
    Orphan,
}

/// Two ticket modes selected at admission; immutable for the orchestrator
/// session lifetime per design.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    SpecDriven,
    NeedsClassify,
}

/// Causes of a state transition, captured per [Req 8.2].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionTrigger {
    /// Tracker emitted a normalized issue (admission or terminal observation).
    TrackerEvent,
    /// Linear assignee changed away from the configured assignee.
    AssignmentLost,
    /// `roki:ready` label removed.
    RokiReadyRemoved,
    /// Orchestrator emitted a parsed `OrchestratorAction`.
    OrchestratorAction,
    /// Phase subprocess emitted a terminal event consumed by the orchestrator.
    PhaseEvent,
    /// Daemon synthesized a `daemon_directive` (stall / retry / fs / orphan).
    DaemonDirective,
    /// Orchestrator process died (crash / unparseable / budget exhausted).
    OrchestratorDead,
    /// Restart-recovery scan reconciled the on-disk world with Linear state.
    RecoveryScan,
    /// Operator-initiated daemon shutdown.
    OperatorShutdown,
}

/// Stable Linear issue identifier. Canonical home: this module. The tracker
/// re-exports through `tracker::model::IssueId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IssueId(pub String);

impl fmt::Display for IssueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for IssueId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for IssueId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// One transition record. `repo` is `None` for pre-worktree transitions
/// (admission, classify). `inactive_reason` is `Some` when `next` is
/// `Inactive`. `correlation_id` ties the record to the structured event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionEvent {
    pub issue: IssueId,
    pub repo: Option<String>,
    pub previous: WorkerState,
    pub next: WorkerState,
    pub trigger: TransitionTrigger,
    pub mode: Option<Mode>,
    pub inactive_reason: Option<InactiveReason>,
    pub correlation_id: String,
}

/// Validation failure on `validate_transition`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error(
        "illegal transition: {previous:?} -> {next:?} via {trigger:?} (not in transition table)"
    )]
    IllegalTransition {
        previous: WorkerState,
        next: WorkerState,
        trigger: TransitionTrigger,
    },
}

/// Validate a candidate transition against the legal-transition table from
/// design.md "Per-issue ticket lifecycle".
///
/// The table forbids `Active -> Cleaning` triggered solely by a `PhaseEvent`:
/// phase subprocess exit alone never enters `Cleaning`. `Cleaning` is
/// reachable only from a `TrackerEvent`, `AssignmentLost`, or
/// `RokiReadyRemoved` (daemon-side stop conditions).
pub fn validate_transition(
    previous: &WorkerState,
    next: &WorkerState,
    trigger: &TransitionTrigger,
) -> Result<(), TransitionError> {
    use TransitionTrigger as T;
    use WorkerState as S;

    let ok = match (previous, next) {
        // Pending -> Active: orchestrator nominated a phase.
        (S::Pending, S::Active) => matches!(trigger, T::OrchestratorAction),

        // Active -> Pending: phase event delivered to orchestrator; orchestrator deliberates.
        (S::Active, S::Pending) => matches!(trigger, T::PhaseEvent),

        // Active -> Backoff: phase_nonclean and retry budget remaining.
        (S::Active, S::Backoff) => matches!(trigger, T::PhaseEvent),

        // Backoff -> Active: backoff timer elapsed and orchestrator re-spawn.
        (S::Backoff, S::Active) => {
            matches!(trigger, T::DaemonDirective | T::OrchestratorAction)
        }

        // Pending -> Inactive: orchestrator action=stop OR orchestrator-dead reasons.
        (S::Pending, S::Inactive(_)) => {
            matches!(trigger, T::OrchestratorAction | T::OrchestratorDead)
        }

        // Active -> Inactive(stall): phase stall with orchestrator dead at detection.
        (S::Active, S::Inactive(reason)) => {
            matches!(reason, InactiveReason::Stall)
                && matches!(trigger, T::DaemonDirective | T::OrchestratorDead)
        }

        // Backoff -> Inactive(retry_exhausted): budget exhausted, orchestrator stops.
        (S::Backoff, S::Inactive(reason)) => {
            matches!(reason, InactiveReason::RetryExhausted)
                && matches!(trigger, T::OrchestratorAction | T::DaemonDirective)
        }

        // Any non-terminal state -> Cleaning: tracker terminal / assignment loss /
        // `roki:ready` removed are the daemon-side stop conditions; recovery scan
        // and operator shutdown additionally reach Cleaning from anywhere.
        // Phase subprocess exit alone (`PhaseEvent`) is explicitly forbidden.
        (
            S::Pending | S::Active | S::Backoff | S::Inactive(_),
            S::Cleaning,
        ) => matches!(
            trigger,
            T::TrackerEvent
                | T::AssignmentLost
                | T::RokiReadyRemoved
                | T::RecoveryScan
                | T::OperatorShutdown
        ),

        // Initial entry: synthetic `(prev=Pending, next=Pending, trigger=TrackerEvent)`
        // is reserved for admission bookkeeping; rejected here so callers must
        // model admission as a real transition into `Pending` from outside.
        _ => false,
    };

    if ok {
        Ok(())
    } else {
        Err(TransitionError::IllegalTransition {
            previous: previous.clone(),
            next: next.clone(),
            trigger: *trigger,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(prev: WorkerState, next: WorkerState, trigger: TransitionTrigger) {
        validate_transition(&prev, &next, &trigger).unwrap_or_else(|err| {
            panic!("expected legal transition, got {err}");
        });
    }

    fn err(prev: WorkerState, next: WorkerState, trigger: TransitionTrigger) {
        let result = validate_transition(&prev, &next, &trigger);
        assert!(matches!(result, Err(TransitionError::IllegalTransition { .. })));
    }

    #[test]
    fn pending_to_active_via_orchestrator_action_is_legal() {
        ok(
            WorkerState::Pending,
            WorkerState::Active,
            TransitionTrigger::OrchestratorAction,
        );
    }

    #[test]
    fn active_to_pending_via_phase_event_is_legal() {
        ok(
            WorkerState::Active,
            WorkerState::Pending,
            TransitionTrigger::PhaseEvent,
        );
    }

    #[test]
    fn active_to_backoff_via_phase_event_is_legal() {
        ok(
            WorkerState::Active,
            WorkerState::Backoff,
            TransitionTrigger::PhaseEvent,
        );
    }

    #[test]
    fn backoff_to_active_via_daemon_directive_is_legal() {
        ok(
            WorkerState::Backoff,
            WorkerState::Active,
            TransitionTrigger::DaemonDirective,
        );
    }

    #[test]
    fn pending_to_inactive_outcomes_via_orchestrator_action_are_legal() {
        for reason in [
            InactiveReason::AwaitingLinear,
            InactiveReason::NeedsOperator,
            InactiveReason::SpecIncomplete,
            InactiveReason::NeedsSplit,
            InactiveReason::AllowlistRejected,
        ] {
            ok(
                WorkerState::Pending,
                WorkerState::Inactive(reason),
                TransitionTrigger::OrchestratorAction,
            );
        }
    }

    #[test]
    fn pending_to_inactive_orchestrator_dead_reasons_are_legal() {
        for reason in [
            InactiveReason::OrchestratorCrash,
            InactiveReason::OrchestratorUnparseable,
            InactiveReason::OrchestratorBudgetExhausted,
        ] {
            ok(
                WorkerState::Pending,
                WorkerState::Inactive(reason),
                TransitionTrigger::OrchestratorDead,
            );
        }
    }

    #[test]
    fn active_to_inactive_stall_via_daemon_directive_is_legal() {
        ok(
            WorkerState::Active,
            WorkerState::Inactive(InactiveReason::Stall),
            TransitionTrigger::DaemonDirective,
        );
    }

    #[test]
    fn backoff_to_inactive_retry_exhausted_via_orchestrator_action_is_legal() {
        ok(
            WorkerState::Backoff,
            WorkerState::Inactive(InactiveReason::RetryExhausted),
            TransitionTrigger::OrchestratorAction,
        );
    }

    #[test]
    fn pending_to_cleaning_via_tracker_event_is_legal() {
        ok(
            WorkerState::Pending,
            WorkerState::Cleaning,
            TransitionTrigger::TrackerEvent,
        );
    }

    #[test]
    fn active_to_cleaning_via_assignment_lost_is_legal() {
        ok(
            WorkerState::Active,
            WorkerState::Cleaning,
            TransitionTrigger::AssignmentLost,
        );
    }

    #[test]
    fn backoff_to_cleaning_via_roki_ready_removed_is_legal() {
        ok(
            WorkerState::Backoff,
            WorkerState::Cleaning,
            TransitionTrigger::RokiReadyRemoved,
        );
    }

    #[test]
    fn inactive_to_cleaning_via_tracker_event_is_legal() {
        ok(
            WorkerState::Inactive(InactiveReason::AwaitingLinear),
            WorkerState::Cleaning,
            TransitionTrigger::TrackerEvent,
        );
    }

    #[test]
    fn active_to_cleaning_via_phase_event_alone_is_illegal() {
        // The forbidden edge: phase subprocess exit alone shall never enter Cleaning.
        err(
            WorkerState::Active,
            WorkerState::Cleaning,
            TransitionTrigger::PhaseEvent,
        );
    }

    #[test]
    fn active_to_pending_via_orchestrator_action_alone_is_illegal() {
        // Active -> Pending requires a phase event delivery, not a bare orchestrator turn.
        err(
            WorkerState::Active,
            WorkerState::Pending,
            TransitionTrigger::OrchestratorAction,
        );
    }

    #[test]
    fn transition_event_carries_mode_and_reason() {
        let event = TransitionEvent {
            issue: IssueId::from("ENG-1"),
            repo: None,
            previous: WorkerState::Pending,
            next: WorkerState::Inactive(InactiveReason::AwaitingLinear),
            trigger: TransitionTrigger::OrchestratorAction,
            mode: Some(Mode::SpecDriven),
            inactive_reason: Some(InactiveReason::AwaitingLinear),
            correlation_id: "corr-1".to_owned(),
        };
        assert_eq!(event.mode, Some(Mode::SpecDriven));
        assert_eq!(
            event.inactive_reason,
            Some(InactiveReason::AwaitingLinear)
        );
    }

    #[test]
    fn issue_id_round_trips_string_and_str() {
        let from_str: IssueId = "ENG-42".into();
        let from_string: IssueId = String::from("ENG-42").into();
        assert_eq!(from_str, from_string);
        assert_eq!(from_str.to_string(), "ENG-42");
    }
}
