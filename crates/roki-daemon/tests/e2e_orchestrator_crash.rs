//! Section 13.6 — Orchestrator session crashes mid-flight.
//!
//! Forces the orchestrator session to exit non-zero before any terminal
//! `action=stop`. The daemon must:
//!
//! - Route the actor to `Inactive(OrchestratorCrash)`.
//! - Avoid any Linear write attempt (no `daemon_directive` is delivered to
//!   stdin because the orchestrator is already dead).
//! - Populate the escalation queue (Req 12.1) so `OrchestratorRead`
//!   surfaces the failure in the TUI.
//! - Preserve the worktree + session tempdir for operator triage.
//!
//! Seam-level synthesis (Option 2): the runtime composition layer is
//! expected to translate a non-zero orchestrator exit without a terminal
//! `action=stop` into BOTH a `DaemonEscalation { OrchestratorCrash, … }`
//! actor message (so the queue is populated per Req 12.3) AND an
//! `OrchestratorActionEvent::ProcessExit` from the session adapter (which
//! drives the state transition out of `Active`). This test drives both
//! signals at the OrchHarness seam to exercise that contract end-to-end
//! without spinning up a real fake_claude orchestrator subprocess. The
//! ordering models the production sequence: the adapter detects the
//! non-zero exit, runtime composition enqueues + routes the directive
//! (orchestrator-dead → queue-only, no Linear write), and the actor
//! observes the ProcessExit on its action channel.
//!
//! Spec refs: requirements.md 12.1, 12.3.

mod common;

use common::OrchHarness;
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::escalation::EscalationKind;
use roki_daemon::orchestrator::read::{OrchestratorRead, OrchestratorReadHandle};
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

    // Runtime composition synthesizes the orchestrator-dead escalation
    // alongside the ProcessExit signal. Sending it through the same actor
    // inbox enqueues the entry (Req 12.1) and is routed as
    // orchestrator-dead → queue-only with no Linear-side directive
    // delivery (Req 12.3).
    h.orchestrator
        .send(
            issue.clone(),
            ActorMessage::DaemonEscalation {
                kind: EscalationKind::OrchestratorCrash,
                fields: serde_json::json!({
                    "repos": [repo.0.clone()],
                    "exit_success": false,
                }),
                correlation_id: format!("crash-{issue}"),
            },
        )
        .await
        .expect("send orchestrator-crash escalation");

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
    drop(delivered);

    // Escalation queue must record the entry, surfaced through the
    // public OrchestratorRead trait (the TUI snapshot path) per Req 12.1.
    let read_handle = OrchestratorReadHandle::new(h.state_map.clone(), h.escalations.clone());
    let snap = read_handle.snapshot();
    let entry = snap
        .escalations
        .iter()
        .find(|e| e.issue == issue)
        .unwrap_or_else(|| {
            panic!("OrchestratorRead snapshot must include the crash entry: {snap:?}")
        });
    assert_eq!(
        entry.kind,
        EscalationKind::OrchestratorCrash,
        "escalation entry must record OrchestratorCrash kind"
    );
    assert!(
        entry.repo.as_deref() == Some(repo.0.as_str()),
        "escalation entry must carry the repo id: {:?}",
        entry.repo
    );

    // Per-issue lookup likewise reports Inactive(OrchestratorCrash).
    let issue_state = read_handle
        .issue(&issue)
        .expect("OrchestratorRead must expose the crashed issue");
    assert!(
        matches!(
            issue_state.state,
            roki_daemon::orchestrator::state::WorkerState::Inactive(
                InactiveReason::OrchestratorCrash
            )
        ),
        "issue state from OrchestratorRead must be Inactive(OrchestratorCrash): {:?}",
        issue_state.state
    );
}
