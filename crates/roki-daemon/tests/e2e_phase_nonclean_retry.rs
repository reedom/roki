//! Section 13.5 — Phase non-clean retry → retry-exhausted end-to-end.
//!
//! The orchestrator core exposes the phase non-clean / retry plumbing
//! through its `PhaseEngine` + `OrchestratorSessionLike` seams. Today the
//! daemon's max-attempts curve + back-off scheduling lives in the runtime
//! composition layer (not yet wired) — this test asserts the behavior the
//! orchestrator core supports today and the test that is expected to land
//! once the retry-window scheduler is composed:
//!
//! - On a non-clean phase exit, the actor stays in a non-Cleaning state
//!   and the orchestrator session continues consuming actions (the actor
//!   does not preempt to `Inactive(retry_exhausted)` itself).
//! - When the orchestrator emits a `daemon_directive(retry_exhausted)`
//!   echo by routing back into the `DaemonEscalation { kind:
//!   RetryExhausted, … }` actor message, the orchestrator session is
//!   still alive — so the directive is delivered to stdin and the
//!   orchestrator's terminal `action=stop outcome=failure` then maps to
//!   `Inactive(RetryExhausted)`.
//!
//! Runtime-wiring TODO: when the retry-window scheduler lands, replace the
//! manual `DaemonEscalation` send with the wall-clock-driven directive
//! emission so the test exercises the back-off curve end-to-end.
//!
//! Spec refs: requirements.md 5.10, 12.2.

mod common;

use common::{phase_complete_event, phase_nonclean_event, run_phase_action, stop_action, OrchHarness};
use roki_daemon::engine::orchestrator_session::action_parser::{Outcome, PhaseName};
use roki_daemon::engine::orchestrator_session::events::NoncleanClassification;
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent, PhaseRunOutcome};
use roki_daemon::orchestrator::escalation::EscalationKind;
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn nonclean_then_directive_then_stop_failure_lands_in_retry_exhausted() {
    let h = OrchHarness::new();

    // First phase run: non-clean (NonZero). Second phase run: clean (the
    // orchestrator nominated a fresh implement after observing the first
    // non-clean event but the retry-budget directive cuts the loop).
    h.phase
        .push_outcome(PhaseRunOutcome::Translated(phase_nonclean_event(
            PhaseName::Implement,
            NoncleanClassification::NonZero,
        )))
        .await;
    h.phase
        .push_outcome(PhaseRunOutcome::Translated(phase_complete_event(
            PhaseName::Implement,
        )))
        .await;

    h.engine
        .push_stream(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
            // After phase nonclean delivered, orchestrator nominates a
            // second implement attempt.
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
            // After the daemon_directive(retry_exhausted) reaches the
            // orchestrator, it emits action=stop outcome=failure.
            OrchestratorActionEvent::Action(stop_action(Outcome::Failure)),
        ])
        .await;

    let issue = IssueId::from("ENG-400");
    let repo = RepoId::from("github.com/owner/repo");
    h.orchestrator
        .send(
            issue.clone(),
            ActorMessage::TrackerAdmit {
                mode: Mode::SpecDriven,
                repo: Some(repo.clone()),
            },
        )
        .await
        .expect("admit");

    // Allow the actor to consume both phase outcomes before injecting the
    // retry-exhausted directive.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if h.phase.invocations.lock().await.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    h.orchestrator
        .send(
            issue.clone(),
            ActorMessage::DaemonEscalation {
                kind: EscalationKind::RetryExhausted,
                fields: serde_json::json!({"attempts": 2, "max_attempts": 2}),
                correlation_id: format!("retry-{issue}"),
            },
        )
        .await
        .expect("send retry-exhausted directive");

    h.wait_for_inactive(&issue, InactiveReason::RetryExhausted)
        .await;

    // Escalation queue must hold the retry_exhausted entry. Poll briefly:
    // the inbox processes messages serially — `DaemonEscalation` is
    // consumed after the actor lands in Inactive(RetryExhausted) (via the
    // Stop action), so the queue insertion lags the state transition by
    // one inbox tick.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let snap = h.escalations.snapshot().await;
        if snap
            .iter()
            .any(|e| e.kind == EscalationKind::RetryExhausted)
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("escalation queue must record retry_exhausted: {snap:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
