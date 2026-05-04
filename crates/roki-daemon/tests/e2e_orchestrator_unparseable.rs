//! Section 13.7 — Orchestrator stdout schema-drift end-to-end.
//!
//! Two consecutive schema-drift turns surface as
//! `OrchestratorActionEvent::TerminalDrift` from the orchestrator-session
//! adapter; the actor must route to `Inactive(OrchestratorUnparseable)`.
//!
//! The raw stdout capture lives in the per-session debug sink; this test
//! asserts the routing contract — the sink contents are covered by the
//! orchestrator-session adapter's own tests.
//!
//! Spec refs: requirements.md 5.4, 12.3.

mod common;

use common::OrchHarness;
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn terminal_drift_routes_to_orchestrator_unparseable() {
    let h = OrchHarness::new();
    h.engine
        .push_stream(vec![OrchestratorActionEvent::TerminalDrift])
        .await;

    let issue = IssueId::from("ENG-600");
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

    h.wait_for_inactive(&issue, InactiveReason::OrchestratorUnparseable)
        .await;

    // Worktree + session tempdir retained for triage.
    assert!(h.worktree.cleanup_calls.lock().await.is_empty());
    assert!(h.session_dirs.remove_calls.lock().unwrap().is_empty());
}
