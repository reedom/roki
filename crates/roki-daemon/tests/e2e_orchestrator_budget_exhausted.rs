//! Section 13.8 — Orchestrator phase budget exhausted end-to-end.
//!
//! `extension.orchestrator.max_phases` caps the number of phase
//! nominations a single orchestrator session may emit. The budget
//! enforcement lives in the orchestrator-session adapter's budget tracker
//! (`engine::orchestrator_session::budget`); when the budget is exhausted
//! the runtime composition layer routes a `daemon_directive` and surfaces
//! a `DaemonEscalation { kind: OrchestratorBudgetExhausted, … }` to the
//! per-issue actor.
//!
//! This test drives the actor with that escalation directly and asserts:
//!
//! - The actor lands in `Inactive(OrchestratorBudgetExhausted)`.
//! - No additional phase invocations occur after the escalation fires
//!   (the actor stops draining further nominations).
//! - The escalation queue records the entry.
//!
//! Runtime-wiring TODO: when the runtime composes the budget tracker into
//! the actor's PhaseEngine seam end-to-end, replace the manual
//! `DaemonEscalation` send with a third `run_phase` action that exceeds
//! `max_phases = 2` to exercise the wall-clock cutoff.
//!
//! Spec refs: requirements.md 5.5, 12.3.

mod common;

use common::{run_phase_action, OrchHarness};
use roki_daemon::engine::orchestrator_session::action_parser::PhaseName;
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::escalation::EscalationKind;
use roki_daemon::orchestrator::state::{InactiveReason, IssueId};

/// `OrchestratorBudgetExhausted` is one of the documented orchestrator-dead
/// escalation kinds. The actor must:
///
/// - Enqueue the escalation entry.
/// - Map to `Inactive(OrchestratorBudgetExhausted)` when no live session
///   is bound (or when the directive arrives before any admit).
/// - Not deliver the directive to a (non-existent) orchestrator stdin.
///
/// The runtime composition layer drives the budget tracker that would
/// emit this directive after a third `run_phase` action exceeds
/// `extension.orchestrator.max_phases`. Until that wiring lands, this
/// test exercises the actor-side mapping by sending the directive
/// directly — exactly the same shape the `OrchestratorEngine` adapter
/// will hand off when the wiring arrives.
#[tokio::test]
async fn budget_exhausted_directive_routes_to_inactive_budget_exhausted() {
    let h = OrchHarness::new();
    // Pre-canned actions for a session that will never launch in this
    // test path; they are kept so that if the wiring evolves to launch
    // first, the assertion still demonstrates "no further phase ran".
    h.engine
        .push_stream(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
        ])
        .await;

    let issue = IssueId::from("ENG-700");

    h.orchestrator
        .send(
            issue.clone(),
            ActorMessage::DaemonEscalation {
                kind: EscalationKind::OrchestratorBudgetExhausted,
                fields: serde_json::json!({"max_phases": 2, "observed": 3}),
                correlation_id: format!("budget-{issue}"),
            },
        )
        .await
        .expect("send budget-exhausted directive");

    h.wait_for_inactive(&issue, InactiveReason::OrchestratorBudgetExhausted)
        .await;

    // Escalation queue records the entry.
    let snap = h.escalations.snapshot().await;
    assert!(
        snap.iter()
            .any(|e| e.kind == EscalationKind::OrchestratorBudgetExhausted),
        "queue must record OrchestratorBudgetExhausted: {snap:?}",
    );

    // No phase ran (no orchestrator was launched).
    let invs = h.phase.invocations.lock().await;
    assert!(invs.is_empty(), "no phase may run after budget cutoff: {invs:?}");
}
