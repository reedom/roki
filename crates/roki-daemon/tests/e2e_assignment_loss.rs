//! Section 13.9 — Assignment lost mid-flight end-to-end.
//!
//! Tracker observes the assignee changing away from the configured
//! viewer mid-implement. The actor must:
//!
//! - Forward a `tracker_terminal(assignment_lost)` to the orchestrator's
//!   stdin so the orchestrator's next turn can record the cancel.
//! - Transition to `Cleaning`.
//! - Iterate the worktree allowlist and remove the worktree (the stub
//!   does so in-test by deleting `<root>/<issue>`).
//! - Remove the session tempdir AFTER worktree cleanup.
//! - Not consume any retry budget (no `RetryExhausted` escalation).
//!
//! Spec refs: requirements.md 3.10, 4.9.

mod common;

use common::{run_phase_action, OrchHarness};
use roki_daemon::engine::orchestrator_session::action_parser::PhaseName;
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::escalation::EscalationKind;
use roki_daemon::orchestrator::state::{IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn assignment_lost_mid_flight_drives_cleaning_without_retry_consumption() {
    let h = OrchHarness::new();
    // The orchestrator nominates one implement; while it runs the tracker
    // observes assignment loss and the actor preempts to Cleaning.
    h.engine
        .push_stream(vec![OrchestratorActionEvent::Action(run_phase_action(
            PhaseName::Implement,
        ))])
        .await;

    let issue = IssueId::from("ENG-800");
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

    // Allow the implement phase to run + the actor to deliver the
    // PhaseComplete back into the (drained-out) action stream.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !h.phase.invocations.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    h.orchestrator
        .send(issue.clone(), ActorMessage::TrackerAssignmentLost)
        .await
        .expect("send assignment-lost");

    // Cleaning is terminal for the actor loop; join().
    h.orchestrator.join(&issue).await;

    // Worktree cleanup ran exactly once.
    let cleanup = h.worktree.cleanup_calls.lock().await;
    assert_eq!(cleanup.as_slice(), std::slice::from_ref(&issue));
    drop(cleanup);

    // Session tempdir removed.
    {
        let session_remove = h.session_dirs.remove_calls.lock().unwrap();
        assert_eq!(session_remove.as_slice(), std::slice::from_ref(&issue));
    }

    // No retry-exhausted escalation: assignment loss does not consume
    // retry budget.
    let snap = h.escalations.snapshot().await;
    assert!(
        snap.iter().all(|e| e.kind != EscalationKind::RetryExhausted),
        "assignment-lost must not consume retry budget: {snap:?}",
    );
}
