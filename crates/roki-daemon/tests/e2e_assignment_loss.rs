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
use roki_daemon::orchestrator::state::{IssueId, Mode, WorkerState};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn assignment_lost_mid_flight_drives_cleaning_without_retry_consumption() {
    let h = OrchHarness::new();
    // Subscribe BEFORE sending any messages so every transition is observed.
    let mut transitions = h.subscribe();
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

    // Drain the transition stream and assert no Backoff transitions
    // occurred (per requirements 3.10 / 4.9: tracker-driven assignment loss
    // preempts to Cleaning without consuming retry budget — the retry
    // window state Backoff must never be entered on this path). Also
    // assert that Cleaning was reached, with the trigger explicitly
    // tagged AssignmentLost (the daemon-side cause of the preempt).
    let mut observed = Vec::new();
    while let Ok(event) = transitions.try_recv() {
        if event.issue == issue {
            observed.push(event);
        }
    }
    assert!(
        !observed
            .iter()
            .any(|e| matches!(e.next, WorkerState::Backoff)
                || matches!(e.previous, WorkerState::Backoff)),
        "assignment-lost must not route through Backoff: {observed:?}",
    );
    let cleaning_via_assignment_lost = observed.iter().any(|e| {
        matches!(e.next, WorkerState::Cleaning)
            && e.trigger
                == roki_daemon::orchestrator::state::TransitionTrigger::AssignmentLost
    });
    assert!(
        cleaning_via_assignment_lost,
        "expected a Cleaning transition triggered by AssignmentLost, got {observed:?}",
    );

    // Branch retention is structurally guaranteed by the WorktreeOps seam:
    // its `cleanup` contract returns the removed worktree paths only and
    // exposes no branch-deletion path (see WorktreeOps trait in
    // orchestrator/core.rs). The cleanup_calls assertion above therefore
    // also witnesses that no branch removal was requested.
}
