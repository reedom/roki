//! Section 13.4 — NEEDS_CLASSIFY Path A end-to-end.
//!
//! Verifies the documented Path A handoff:
//!
//! - Admit with `Mode::NeedsClassify`.
//! - Orchestrator nominates `classify`; phase returns
//!   `phase_complete(success)` with `classify.path = a`.
//! - Orchestrator's next turn emits `action=stop outcome=needs_operator`
//!   along with the Linear comment + label that route the ticket back to
//!   the operator (the `linear_writes` field).
//! - Actor lands in `Inactive(NeedsOperator)`; worktree + session tempdir
//!   are preserved.
//!
//! Spec refs: requirements.md 4.4, 4.11, 5.11, 7.2.

mod common;

use common::{run_phase_action, OrchHarness};
use roki_daemon::engine::orchestrator_session::action_parser::{
    ActionKind, BoundedString200, LinearWriteAck, OrchestratorAction, Outcome, PhaseName,
};
use roki_daemon::engine::orchestrator_session::events::{
    ClassifyOutcome, ClassifyPath, DaemonEvent, PhaseCompletePayload,
};
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent, PhaseRunOutcome};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn needs_classify_path_a_lands_in_needs_operator_and_preserves_worktree() {
    let h = OrchHarness::new();

    h.phase
        .push_outcome(PhaseRunOutcome::Translated(DaemonEvent::PhaseComplete(
            PhaseCompletePayload {
                phase: PhaseName::Classify,
                result: serde_json::json!({"subtype": "success"}),
                pr_url: None,
                review_artifact_path: None,
                classify: Some(ClassifyOutcome {
                    path: ClassifyPath::A,
                    suggested_command: Some("/kiro-spec-init".to_owned()),
                    suggested_label: Some("needs-spec".to_owned()),
                    target_feature: Some("auth-module".to_owned()),
                }),
            },
        )))
        .await;

    let stop_with_writes = OrchestratorAction {
        action: ActionKind::Stop,
        phase: None,
        additional_context: None,
        outcome: Some(Outcome::NeedsOperator),
        linear_writes: Some(vec![
            LinearWriteAck::Label("needs-spec".to_owned()),
            LinearWriteAck::CommentPosted("comment-id-1".to_owned()),
        ]),
        reason: BoundedString200::new("Path A: needs spec authoring").unwrap(),
    };

    h.engine
        .push_stream(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Classify)),
            OrchestratorActionEvent::Action(stop_with_writes),
        ])
        .await;

    let issue = IssueId::from("ENG-300");
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

    h.wait_for_inactive(&issue, InactiveReason::NeedsOperator)
        .await;

    // Worktree cleanup must NOT have been invoked (Inactive(NeedsOperator)
    // preserves the residue per Req 4.11).
    let cleanup = h.worktree.cleanup_calls.lock().await;
    assert!(cleanup.is_empty(), "NeedsOperator preserves the worktree");

    // Session tempdir not removed either.
    let session_remove = h.session_dirs.remove_calls.lock().unwrap();
    assert!(
        session_remove.is_empty(),
        "NeedsOperator preserves the session tempdir"
    );
}
