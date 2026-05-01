//! Orchestrator state, transition table, and transition-event types.
//!
//! This module is the foundation type layer for the per-`(issue,)` state
//! machine described in design.md "Per-issue worker lifecycle" and pinned by
//! requirements 8.1, 8.2, and 13.2. Everything here is pure: no I/O, no async,
//! no shared state. Downstream submodules (`event_bus`, `hooks`, `worker`,
//! `read`) consume these types but never import from each other through this
//! module.
//!
//! ## What lives here
//!
//! * [`WorkerState`] — every state a per-issue worker can occupy, including
//!   the [`WorkerState::Cleaning`] interim state that gives downstream specs
//!   (notably roki-distill-postmerge) a stable place to do deferred work
//!   while the workspace still exists. Workspace removal happens only on
//!   `Cleaning -> [*]`; the terminal-end is expressed as the absence of
//!   legal outgoing transitions from `Cleaning` and `TerminalFailure`.
//! * [`TransitionTrigger`] — the source that caused the transition. Only the
//!   sources design.md authorises the orchestrator to drive transitions from
//!   are listed: tracker events, engine events, recovery scan, operator
//!   shutdown, and subscriber-veto bookkeeping.
//! * [`VetoDecision`] — the `Allow` / `Deny { reason }` payload returned by
//!   pre-cleanup hooks and by vetoable subscribers. Carrying the reason lets
//!   the orchestrator log a denied transition with operator-actionable detail.
//! * [`TransitionEvent`] — the structured payload published on every
//!   transition. The `vetoable` field is a derived flag the orchestrator
//!   reads off the transition pair so subscribers and observability pipelines
//!   do not have to reimplement the table.
//! * [`legal_transition`] / [`is_vetoable`] / [`TransitionEvent::new`] — the
//!   pure helpers that encode the table. They are exhaustively unit-tested at
//!   the bottom of this file.
//!
//! ## Vetoable subset
//!
//! Per design.md and requirements 8.3 / 13.2 the vetoable subset is hard-coded
//! for the MVP and contains exactly three transitions:
//!
//! * `Queued -> Active`            — consumed by roki-spec-gate
//! * `AwaitingReview -> TerminalSuccess` — consumed by roki-review-gate
//! * `TerminalSuccess -> Cleaning` — consumed by roki-distill-postmerge as
//!   the pre-cleanup hook
//!
//! Every other legal transition is observable but non-vetoable.
//!
//! ## Per-task-7.1b note: state-machine key collapse
//!
//! Task 7.1b collapsed the orchestrator key from `(repo, issue)` to `(issue,)`.
//! The `RepoId` newtype is retained here so 7.1d can key the
//! `WorktreeRegistry` by it; nothing in the state machine itself reads the
//! repo any more. `TransitionEvent` no longer carries a `repo` field — repo
//! association is owned by the (yet-to-land) `WorktreeRegistry` and is per
//! worktree the agent opens, not per state-machine transition.

use uuid::Uuid;

/// Repository identifier as carried through the `WorktreeRegistry` (added by
/// task 7.1d).
///
/// Defined here (rather than in a shared id module) so `orchestrator/state` is
/// self-contained; later tasks may relocate the type without changing its
/// shape, and downstream code already pattern-matches on `String`-equivalent
/// newtypes only. The state machine itself does not key by `RepoId`; the type
/// stays public for the agent-driven worktree allowlist that 7.1d wires.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoId(String);

impl RepoId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Linear issue identifier (e.g. `ENG-42`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IssueId(String);

impl IssueId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Per-worker correlation identifier; one value per orchestrator-driven
/// invocation of a worker, threaded through structured logs and events.
///
/// Wraps a [`Uuid`] so the orchestrator can mint a fresh value on each launch
/// without coupling callers to the concrete generator. Construction is
/// explicit via [`CorrelationId::new`] / [`CorrelationId::from_uuid`] so that
/// test code can pin a deterministic value when asserting event shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CorrelationId(Uuid);

impl CorrelationId {
    /// Mint a fresh correlation id (UUID v4).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a caller-provided UUID. Used by tests and recovery reconciliation.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

/// Every state a per-issue worker can occupy.
///
/// Mirrors the lifecycle diagram in design.md. Variants are deliberately flat
/// (no payload) so the state itself never carries hidden context — all
/// per-transition detail rides on [`TransitionEvent`]. Downstream code is
/// expected to match exhaustively (no `_` arms) so that adding a new state
/// here forces every consumer to acknowledge it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerState {
    /// Issue first observed by the tracker, not yet queued.
    Discovered,
    /// Routed and waiting for a worker slot / spec gate.
    Queued,
    /// Worker subprocess running (or in continuation retry).
    Active,
    /// PR open; waiting for reviewer or tracker move to terminal success.
    AwaitingReview,
    /// Backoff window between worker launches.
    Backoff,
    /// Event-inactivity exceeded the stall window; worker terminated.
    Stalled,
    /// Tracker reports the issue resolved; pre-cleanup hooks have not yet run.
    TerminalSuccess,
    /// Interim state between `TerminalSuccess` and workspace removal. The
    /// pre-cleanup hook target — workspace removal happens only on
    /// `Cleaning -> [*]`.
    Cleaning,
    /// Max retries exceeded or operator intervention; workspace retained.
    TerminalFailure,
}

/// Source authorised to drive a transition. The orchestrator treats every
/// transition whose trigger is not in this set as a programmer error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionTrigger {
    /// Normalized issue event from the tracker (webhook or polling).
    TrackerEvent,
    /// Engine lifecycle event (subprocess started, exited, stalled, ...).
    EngineEvent,
    /// Restart-time reconciliation against Linear and the workspace layout.
    RecoveryScan,
    /// SIGINT / SIGTERM handler asking workers to wind down.
    OperatorShutdown,
    /// A vetoable transition was denied by a subscriber or pre-cleanup hook;
    /// the orchestrator records the denial event with this trigger so logs
    /// distinguish a "subscriber said no" from a "tracker said move".
    SubscriberVeto,
}

/// Result of a vetoable subscriber or pre-cleanup hook.
///
/// `Deny` carries an operator-readable reason so the orchestrator can log a
/// denied transition with actionable detail (and roki-observability can
/// surface it through the read API later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VetoDecision {
    Allow,
    Deny { reason: String },
}

impl VetoDecision {
    pub fn deny(reason: impl Into<String>) -> Self {
        VetoDecision::Deny {
            reason: reason.into(),
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, VetoDecision::Allow)
    }
}

/// Structured payload published for every committed transition.
///
/// `vetoable` is a derived flag — the orchestrator computes it from the
/// `(previous, next)` pair using [`is_vetoable`] when constructing the event.
/// Carrying the flag on the event lets observers and structured-log pipelines
/// avoid reimplementing the vetoable table.
///
/// Per task 7.1b the state-machine key collapsed from `(repo, issue)` to
/// `(issue,)`; the event therefore carries only the issue. Repo association
/// (post-task-7.1d) lives on the `WorktreeRegistry` per opened worktree, not
/// on the per-transition event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionEvent {
    pub issue: IssueId,
    pub previous: WorkerState,
    pub next: WorkerState,
    pub trigger: TransitionTrigger,
    pub correlation_id: CorrelationId,
    /// `true` iff `(previous, next)` is one of the three vetoable transitions.
    pub vetoable: bool,
}

impl TransitionEvent {
    /// Build a transition event for a legal transition.
    ///
    /// Returns `None` if `(previous, next)` is not a legal transition. Callers
    /// in the orchestrator are expected to have already gated on
    /// [`legal_transition`] so an `Err`-equivalent here is a programmer error;
    /// returning `Option` keeps this function pure and panic-free for testing.
    pub fn new(
        issue: IssueId,
        previous: WorkerState,
        next: WorkerState,
        trigger: TransitionTrigger,
        correlation_id: CorrelationId,
    ) -> Option<Self> {
        if !legal_transition(previous, next) {
            return None;
        }
        Some(Self {
            issue,
            previous,
            next,
            trigger,
            correlation_id,
            vetoable: is_vetoable(previous, next),
        })
    }
}

/// Returns `true` iff the `(from, to)` transition is part of the documented
/// state-machine table.
///
/// The table is encoded as an exhaustive `match` so adding a new state forces
/// the compiler to flag every missing entry — there is no catch-all arm.
pub const fn legal_transition(from: WorkerState, to: WorkerState) -> bool {
    use WorkerState::*;
    match (from, to) {
        // Discovery and queueing.
        (Discovered, Queued) => true,
        (Queued, Active) => true,
        // Failure path before a worker ever runs (e.g. unrouteable issue).
        (Queued, TerminalFailure) => true,

        // Active worker lifecycle.
        (Active, Active) => true, // continuation retry on clean exit
        (Active, AwaitingReview) => true,
        (Active, Backoff) => true,
        (Active, Stalled) => true,
        (Active, TerminalFailure) => true,

        // Backoff and stall recovery.
        (Backoff, Active) => true,
        (Stalled, Backoff) => true,

        // Review loop.
        (AwaitingReview, TerminalSuccess) => true,
        (AwaitingReview, Active) => true,

        // Cleanup interim and terminal-end.
        (TerminalSuccess, Cleaning) => true,
        // Cleaning has no outgoing transition: workspace removal terminates
        // the per-issue lifecycle (Cleaning -> [*]).
        // TerminalFailure has no outgoing transition: workspace retained for
        // operator inspection (TerminalFailure -> [*]).
        _ => false,
    }
}

/// Returns `true` iff the `(from, to)` transition is in the vetoable subset.
///
/// Subscribers and pre-cleanup hooks may return [`VetoDecision::Deny`] only
/// for transitions where this returns `true`. A `Deny` returned for any other
/// transition is treated as a programmer error by the orchestrator (logged
/// and ignored); the orchestrator never asks subscribers to vote on
/// non-vetoable transitions in the first place.
pub const fn is_vetoable(from: WorkerState, to: WorkerState) -> bool {
    use WorkerState::*;
    matches!(
        (from, to),
        (Queued, Active) | (AwaitingReview, TerminalSuccess) | (TerminalSuccess, Cleaning)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every transition pair documented by design.md "Per-issue worker
    /// lifecycle". `expected_legal` is the table; `expected_vetoable` flags the
    /// three transitions consumed by spec-gate, review-gate, and
    /// distill-postmerge respectively.
    fn documented_transitions() -> Vec<(WorkerState, WorkerState, bool, bool)> {
        use WorkerState::*;
        vec![
            // Discovery and queueing.
            (Discovered, Queued, true, false),
            (Queued, Active, true, true),
            (Queued, TerminalFailure, true, false),
            // Active lifecycle.
            (Active, Active, true, false),
            (Active, AwaitingReview, true, false),
            (Active, Backoff, true, false),
            (Active, Stalled, true, false),
            (Active, TerminalFailure, true, false),
            // Backoff / stall recovery.
            (Backoff, Active, true, false),
            (Stalled, Backoff, true, false),
            // Review loop.
            (AwaitingReview, TerminalSuccess, true, true),
            (AwaitingReview, Active, true, false),
            // Cleanup interim.
            (TerminalSuccess, Cleaning, true, true),
        ]
    }

    /// All `WorkerState` variants — used to assert that any pair not present
    /// in the documented table is rejected by `legal_transition`.
    fn all_states() -> Vec<WorkerState> {
        use WorkerState::*;
        vec![
            Discovered,
            Queued,
            Active,
            AwaitingReview,
            Backoff,
            Stalled,
            TerminalSuccess,
            Cleaning,
            TerminalFailure,
        ]
    }

    #[test]
    fn legal_transition_matches_documented_table() {
        for (from, to, expected_legal, _) in documented_transitions() {
            assert!(
                legal_transition(from, to),
                "expected legal transition {from:?} -> {to:?} to be allowed",
            );
            assert_eq!(
                expected_legal,
                legal_transition(from, to),
                "transition {from:?} -> {to:?}",
            );
        }
    }

    #[test]
    fn legal_transition_rejects_undocumented_pairs() {
        let documented: std::collections::HashSet<(WorkerState, WorkerState)> =
            documented_transitions()
                .into_iter()
                .map(|(from, to, _, _)| (from, to))
                .collect();

        for from in all_states() {
            for to in all_states() {
                let pair = (from, to);
                let documented_pair = documented.contains(&pair);
                assert_eq!(
                    documented_pair,
                    legal_transition(from, to),
                    "legal_transition({from:?}, {to:?}) disagrees with documented table",
                );
            }
        }
    }

    #[test]
    fn cleaning_and_terminal_failure_are_terminal_ends() {
        for to in all_states() {
            assert!(
                !legal_transition(WorkerState::Cleaning, to),
                "Cleaning must have no outgoing transition (Cleaning -> [*]); found Cleaning -> {to:?}",
            );
            assert!(
                !legal_transition(WorkerState::TerminalFailure, to),
                "TerminalFailure must have no outgoing transition; found TerminalFailure -> {to:?}",
            );
        }
    }

    #[test]
    fn is_vetoable_marks_exactly_three_transitions() {
        let vetoable_pairs: Vec<(WorkerState, WorkerState)> = documented_transitions()
            .into_iter()
            .filter(|(_, _, _, vetoable)| *vetoable)
            .map(|(from, to, _, _)| (from, to))
            .collect();
        assert_eq!(
            vetoable_pairs,
            vec![
                (WorkerState::Queued, WorkerState::Active),
                (WorkerState::AwaitingReview, WorkerState::TerminalSuccess),
                (WorkerState::TerminalSuccess, WorkerState::Cleaning),
            ],
            "the vetoable subset must match design.md exactly",
        );
        for (from, to) in vetoable_pairs {
            assert!(is_vetoable(from, to), "{from:?} -> {to:?} must be vetoable");
        }
    }

    #[test]
    fn is_vetoable_returns_false_for_every_other_pair() {
        let vetoable_set: std::collections::HashSet<(WorkerState, WorkerState)> = [
            (WorkerState::Queued, WorkerState::Active),
            (WorkerState::AwaitingReview, WorkerState::TerminalSuccess),
            (WorkerState::TerminalSuccess, WorkerState::Cleaning),
        ]
        .into_iter()
        .collect();

        for from in all_states() {
            for to in all_states() {
                let expected = vetoable_set.contains(&(from, to));
                assert_eq!(
                    expected,
                    is_vetoable(from, to),
                    "is_vetoable({from:?}, {to:?}) disagrees with the documented vetoable subset",
                );
            }
        }
    }

    #[test]
    fn transition_event_new_populates_every_documented_transition() {
        let issue = IssueId::new("ENG-1");
        let correlation = CorrelationId::from_uuid(Uuid::nil());

        for (from, to, _, expected_vetoable) in documented_transitions() {
            let event = TransitionEvent::new(
                issue.clone(),
                from,
                to,
                TransitionTrigger::TrackerEvent,
                correlation,
            )
            .unwrap_or_else(|| panic!("expected event for legal transition {from:?} -> {to:?}"));

            assert_eq!(event.previous, from, "previous for {from:?} -> {to:?}");
            assert_eq!(event.next, to, "next for {from:?} -> {to:?}");
            assert_eq!(
                event.vetoable, expected_vetoable,
                "vetoable flag for {from:?} -> {to:?}",
            );
            assert_eq!(event.issue, issue);
            assert_eq!(event.trigger, TransitionTrigger::TrackerEvent);
            assert_eq!(event.correlation_id, correlation);
        }
    }

    #[test]
    fn transition_event_new_rejects_illegal_transitions() {
        // A representative illegal transition: Active back to Discovered.
        let issue = IssueId::new("ENG-1");
        let event = TransitionEvent::new(
            issue,
            WorkerState::Active,
            WorkerState::Discovered,
            TransitionTrigger::TrackerEvent,
            CorrelationId::new(),
        );
        assert!(
            event.is_none(),
            "Active -> Discovered must not produce a TransitionEvent",
        );
    }

    #[test]
    fn transition_event_carries_vetoable_flag_for_each_vetoable_transition() {
        // Tightly focused assertion the task calls out: every one of the
        // three vetoable transitions produces an event with `vetoable = true`
        // and the correct previous/next pair. This is the observable
        // completion criterion stated in tasks.md task 2.1.
        let issue = IssueId::new("ENG-vet");
        let correlation = CorrelationId::from_uuid(Uuid::nil());

        let cases = [
            (WorkerState::Queued, WorkerState::Active),
            (WorkerState::AwaitingReview, WorkerState::TerminalSuccess),
            (WorkerState::TerminalSuccess, WorkerState::Cleaning),
        ];

        for (from, to) in cases {
            let event = TransitionEvent::new(
                issue.clone(),
                from,
                to,
                TransitionTrigger::TrackerEvent,
                correlation,
            )
            .expect("vetoable transition must be legal");
            assert!(event.vetoable, "{from:?} -> {to:?} must be vetoable");
            assert_eq!(event.previous, from);
            assert_eq!(event.next, to);
        }
    }

    #[test]
    fn veto_decision_helpers_round_trip() {
        let allow = VetoDecision::Allow;
        assert!(allow.is_allow());
        let deny = VetoDecision::deny("spec gate not satisfied");
        assert!(!deny.is_allow());
        match deny {
            VetoDecision::Deny { reason } => {
                assert_eq!(reason, "spec gate not satisfied");
            }
            VetoDecision::Allow => panic!("expected Deny"),
        }
    }
}
