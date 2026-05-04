//! Section 13.10 — review.md structural validation drives a retry.
//!
//! The orchestrator's responsibility is to read `review.md`, validate its
//! structural status, and either re-nominate `implement` (with the failing
//! per-criterion entries serialized into `additional_context`) or proceed
//! to `validate`/`open_pr`. This test asserts the daemon-side surface that
//! supports that orchestrator behavior:
//!
//! - When the orchestrator emits `run_phase=implement` with a non-empty
//!   `additional_context`, the actor delivers that context verbatim into
//!   the phase engine invocation.
//! - The eventual `action=stop outcome=success` lands in
//!   `Inactive(AwaitingLinear)`.
//!
//! Runtime-wiring + orchestrator-prompt TODO: the structural validation of
//! `review.md` itself runs inside the orchestrator session, not the
//! daemon. The orchestrator-prompt rendering layer (Task 7.x) emits the
//! validation grammar; this test exercises the daemon's contract that
//! whatever `additional_context` the orchestrator hands back is
//! round-tripped to the phase subprocess.
//!
//! Spec refs: requirements.md 4.4, 13.4.

mod common;

use common::{
    phase_complete_event, run_phase_action, run_phase_action_with_context, stop_action,
    OrchHarness,
};
use roki_daemon::engine::orchestrator_session::action_parser::{Outcome, PhaseName};
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent, PhaseRunOutcome};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn implement_retry_with_review_failures_round_trips_additional_context() {
    let h = OrchHarness::new();

    let failing_criteria = "criterion #2 unmet: handler does not redact webhook secret";

    // Pre-canned phase outcomes for: implement, review, finalize_review,
    // implement (retry), review (retry), finalize_review (retry).
    for phase in [
        PhaseName::Implement,
        PhaseName::Review,
        PhaseName::FinalizeReview,
        PhaseName::Implement,
        PhaseName::Review,
        PhaseName::FinalizeReview,
    ] {
        h.phase
            .push_outcome(PhaseRunOutcome::Translated(phase_complete_event(phase)))
            .await;
    }

    h.engine
        .push_stream(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Review)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::FinalizeReview)),
            // First review.md validation failed — orchestrator nominates a
            // retry with the failing-criterion summary as additional_context.
            OrchestratorActionEvent::Action(run_phase_action_with_context(
                PhaseName::Implement,
                failing_criteria,
            )),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Review)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::FinalizeReview)),
            OrchestratorActionEvent::Action(stop_action(Outcome::Success)),
        ])
        .await;

    let issue = IssueId::from("ENG-900");
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

    h.wait_for_inactive(&issue, InactiveReason::AwaitingLinear)
        .await;

    let invs = h.phase.invocations.lock().await;
    assert_eq!(invs.len(), 6, "two implement+review+finalize cycles");
    // The retry implement must carry the failing-criterion summary.
    assert_eq!(
        invs[3].phase,
        PhaseName::Implement,
        "fourth invocation is the retry implement"
    );
    assert_eq!(
        invs[3].additional_context.as_deref(),
        Some(failing_criteria),
        "retry implement must round-trip the failing-criterion context"
    );
}
