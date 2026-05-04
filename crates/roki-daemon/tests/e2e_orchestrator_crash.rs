//! Section 13.6 — Orchestrator session crashes mid-flight.
//!
//! Forces the orchestrator session to exit non-zero before any terminal
//! `action=stop`. The daemon must:
//!
//! - Route the actor to `Inactive(OrchestratorCrash)`.
//! - Avoid any Linear write attempt (no `daemon_directive` is delivered to
//!   stdin because the orchestrator is already dead).
//! - Preserve the worktree + session tempdir for operator triage.
//!
//! Drives the actor via the `OrchestratorActionEvent::ProcessExit` event,
//! which is what the production orchestrator-session adapter emits when
//! the child process exits without a terminal stop.
//!
//! Spec refs: requirements.md 12.3.

mod common;

use common::OrchHarness;
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn orchestrator_process_exit_routes_to_orchestrator_crash() {
    let h = OrchHarness::new();
    h.engine
        .push_stream(vec![OrchestratorActionEvent::ProcessExit { success: false }])
        .await;

    let issue = IssueId::from("ENG-500");
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

    h.wait_for_inactive(&issue, InactiveReason::OrchestratorCrash)
        .await;

    // Worktree retained — operator triages.
    let cleanup = h.worktree.cleanup_calls.lock().await;
    assert!(
        cleanup.is_empty(),
        "OrchestratorCrash must preserve worktree for triage"
    );

    // Session tempdir retained.
    {
        let session_remove = h.session_dirs.remove_calls.lock().unwrap();
        assert!(
            session_remove.is_empty(),
            "OrchestratorCrash must preserve session tempdir for triage"
        );
    }

    // No DaemonDirective delivered: there's no live orchestrator stdin.
    let delivered = h.engine.delivered.lock().await;
    let has_directive = delivered.iter().any(|e| {
        matches!(
            e,
            roki_daemon::engine::orchestrator_session::events::DaemonEvent::DaemonDirective(_)
        )
    });
    assert!(
        !has_directive,
        "no daemon_directive may be sent when orchestrator already dead"
    );
}
