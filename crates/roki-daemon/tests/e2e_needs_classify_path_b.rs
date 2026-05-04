//! Section 13.3 — NEEDS_CLASSIFY Path B end-to-end.
//!
//! Verifies the documented Path B happy path:
//!
//! - Admit with `Mode::NeedsClassify`.
//! - Orchestrator's first turn nominates `classify`.
//! - Phase returns `phase_complete(success)` with `classify.path = b`.
//! - Orchestrator's next turns nominate `implement` (with the verbatim
//!   acceptance criteria carried as `additional_context`) → `review` →
//!   `validate` → `open_pr` → `finalize_review`.
//! - Orchestrator emits `action=stop outcome=success`.
//! - Actor lands in `Inactive(AwaitingLinear)`.
//!
//! Spec refs: requirements.md 4.4, 5.6.

mod common;

use common::{
    phase_complete_event, run_phase_action, run_phase_action_with_context, stop_action,
    OrchHarness,
};
use roki_daemon::engine::orchestrator_session::action_parser::{Outcome, PhaseName};
use roki_daemon::engine::orchestrator_session::events::{
    ClassifyOutcome, ClassifyPath, DaemonEvent, PhaseCompletePayload,
};
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent, PhaseRunOutcome};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn needs_classify_path_b_drives_implement_with_additional_context() {
    let h = OrchHarness::new();

    let acceptance_criteria = "- behavior X must hold\n- output Y";

    // Phase outcomes: classify returns Path B, all others return success.
    h.phase
        .push_outcome(PhaseRunOutcome::Translated(DaemonEvent::PhaseComplete(
            PhaseCompletePayload {
                phase: PhaseName::Classify,
                result: serde_json::json!({"subtype": "success"}),
                pr_url: None,
                review_artifact_path: None,
                classify: Some(ClassifyOutcome {
                    path: ClassifyPath::B,
                    suggested_command: None,
                    suggested_label: None,
                    target_feature: None,
                }),
            },
        )))
        .await;
    for phase in [
        PhaseName::Implement,
        PhaseName::Review,
        PhaseName::Validate,
        PhaseName::OpenPr,
        PhaseName::FinalizeReview,
    ] {
        h.phase
            .push_outcome(PhaseRunOutcome::Translated(phase_complete_event(phase)))
            .await;
    }

    h.engine
        .push_stream(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Classify)),
            OrchestratorActionEvent::Action(run_phase_action_with_context(
                PhaseName::Implement,
                acceptance_criteria,
            )),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Review)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Validate)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::OpenPr)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::FinalizeReview)),
            OrchestratorActionEvent::Action(stop_action(Outcome::Success)),
        ])
        .await;

    let issue = IssueId::from("ENG-200");
    let repo = RepoId::from("github.com/owner/repo");
    h.orchestrator
        .send(
            issue.clone(),
            ActorMessage::TrackerAdmit {
                mode: Mode::NeedsClassify,
                repo: Some(repo.clone()),
            },
        )
        .await
        .expect("admit");

    h.wait_for_inactive(&issue, InactiveReason::AwaitingLinear)
        .await;

    let invs = h.phase.invocations.lock().await;
    // Classify ran first with no worktree (catalog-required), then five
    // implement-shape phases each with a worktree.
    assert_eq!(invs.len(), 6);
    assert_eq!(invs[0].phase, PhaseName::Classify);
    assert!(
        invs[0].worktree_path.is_none(),
        "classify must not be assigned a worktree"
    );
    assert_eq!(invs[1].phase, PhaseName::Implement);
    assert!(invs[1].worktree_path.is_some());
    assert_eq!(
        invs[1].additional_context.as_deref(),
        Some(acceptance_criteria),
        "implement must receive the verbatim acceptance criteria"
    );
}
