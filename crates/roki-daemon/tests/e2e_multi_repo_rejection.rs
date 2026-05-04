//! Section 13.11 — Multi-repo classify split / allowlist rejection.
//!
//! When the orchestrator detects classify Path B context that names two
//! repos (or an out-of-allowlist repo) it emits one of:
//!
//! - `action=stop outcome=needs_split` (multi-repo): daemon maps to
//!   `Inactive(NeedsSplit)`.
//! - `action=stop outcome=allowlist_rejected` (out-of-allowlist): daemon
//!   maps to `Inactive(AllowlistRejected)`.
//!
//! In either case the orchestrator posts a Linear comment in the same
//! turn (carried in the `linear_writes` field of the stop action). This
//! test asserts the daemon-side mapping for both outcomes.
//!
//! Spec refs: requirements.md 4.5.

mod common;

use common::OrchHarness;
use roki_daemon::engine::orchestrator_session::action_parser::{
    ActionKind, BoundedString200, LinearWriteAck, OrchestratorAction, Outcome,
};
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

fn stop_with_writes(outcome: Outcome, comment_id: &str) -> OrchestratorAction {
    OrchestratorAction {
        action: ActionKind::Stop,
        phase: None,
        additional_context: None,
        outcome: Some(outcome),
        linear_writes: Some(vec![LinearWriteAck::CommentPosted(comment_id.to_owned())]),
        reason: BoundedString200::new("rejection routed").unwrap(),
    }
}

#[tokio::test]
async fn outcome_needs_split_lands_in_inactive_needs_split() {
    let h = OrchHarness::new();
    h.engine
        .push_stream(vec![OrchestratorActionEvent::Action(stop_with_writes(
            Outcome::NeedsSplit,
            "comment-split",
        ))])
        .await;

    let issue = IssueId::from("ENG-A100");
    h.orchestrator
        .send(
            issue.clone(),
            ActorMessage::TrackerAdmit {
                mode: Mode::NeedsClassify,
                repo: Some(RepoId::from("github.com/owner/repo-a")),
            },
        )
        .await
        .expect("admit");

    h.wait_for_inactive(&issue, InactiveReason::NeedsSplit).await;

    // Worktree + session retained for operator triage.
    assert!(h.worktree.cleanup_calls.lock().await.is_empty());
    assert!(h.session_dirs.remove_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn outcome_allowlist_rejected_lands_in_inactive_allowlist_rejected() {
    let h = OrchHarness::new();
    h.engine
        .push_stream(vec![OrchestratorActionEvent::Action(stop_with_writes(
            Outcome::AllowlistRejected,
            "comment-allowlist",
        ))])
        .await;

    let issue = IssueId::from("ENG-A101");
    h.orchestrator
        .send(
            issue.clone(),
            ActorMessage::TrackerAdmit {
                mode: Mode::NeedsClassify,
                repo: Some(RepoId::from("github.com/other/repo")),
            },
        )
        .await
        .expect("admit");

    h.wait_for_inactive(&issue, InactiveReason::AllowlistRejected)
        .await;

    assert!(h.worktree.cleanup_calls.lock().await.is_empty());
    assert!(h.session_dirs.remove_calls.lock().unwrap().is_empty());
}
