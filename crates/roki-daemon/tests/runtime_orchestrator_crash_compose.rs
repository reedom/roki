//! Task 10.8 — production path enqueues `EscalationEntry` on orchestrator
//! ProcessExit / TerminalDrift.
//!
//! The 13.6 / 13.7 seam tests (`e2e_orchestrator_crash`,
//! `e2e_orchestrator_unparseable`) drive BOTH the action event and the
//! `ActorMessage::DaemonEscalation` through the harness inbox to model the
//! contract-level coverage. This test fixes the production gap: a non-zero
//! orchestrator exit (or a terminal schema drift) must enqueue the
//! `EscalationEntry` on its own — without any paired runtime-synthesized
//! `DaemonEscalation` — so the OrchestratorRead snapshot surfaces the
//! failure even when the runtime does not synthesize a separate signal.
//!
//! Spec refs: requirements.md Req 12.1, 12.3.

mod common;

use common::OrchHarness;
use roki_daemon::engine::orchestrator_session::events::DaemonEvent;
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::escalation::EscalationKind;
use roki_daemon::orchestrator::read::{OrchestratorRead, OrchestratorReadHandle};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode, WorkerState};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn process_exit_alone_enqueues_orchestrator_crash_entry() {
    let h = OrchHarness::new();
    h.engine
        .push_stream(vec![OrchestratorActionEvent::ProcessExit { success: false }])
        .await;

    let issue = IssueId::from("ENG-810");
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

    // Note: NO paired ActorMessage::DaemonEscalation is sent. The production
    // core handler must enqueue on its own.

    h.wait_for_inactive(&issue, InactiveReason::OrchestratorCrash)
        .await;

    // (a) Inactive(OrchestratorCrash) reached — already verified by wait.

    // (b) The escalation queue contains the OrchestratorCrash entry for this
    //     issue, surfaced through the OrchestratorRead trait.
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
    assert!(
        !entry.correlation_id.is_empty(),
        "correlation_id must be set"
    );

    // (c) No Linear write side effects: orchestrator-dead → no daemon_directive
    //     delivery to the (already-dead) session stdin.
    let delivered = h.engine.delivered.lock().await;
    let has_directive = delivered
        .iter()
        .any(|e| matches!(e, DaemonEvent::DaemonDirective(_)));
    assert!(
        !has_directive,
        "orchestrator-dead path must not attempt a Linear write"
    );

    // Per-issue lookup likewise reports Inactive(OrchestratorCrash).
    let issue_state = read_handle
        .issue(&issue)
        .expect("OrchestratorRead must expose the crashed issue");
    assert!(
        matches!(
            issue_state.state,
            WorkerState::Inactive(InactiveReason::OrchestratorCrash)
        ),
        "issue state must be Inactive(OrchestratorCrash): {:?}",
        issue_state.state
    );
}

#[tokio::test]
async fn terminal_drift_alone_enqueues_orchestrator_unparseable_entry() {
    let h = OrchHarness::new();
    h.engine
        .push_stream(vec![OrchestratorActionEvent::TerminalDrift])
        .await;

    let issue = IssueId::from("ENG-820");
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

    let read_handle = OrchestratorReadHandle::new(h.state_map.clone(), h.escalations.clone());
    let snap = read_handle.snapshot();
    let entry = snap
        .escalations
        .iter()
        .find(|e| e.issue == issue)
        .unwrap_or_else(|| {
            panic!("OrchestratorRead snapshot must include the unparseable entry: {snap:?}")
        });
    assert_eq!(
        entry.kind,
        EscalationKind::OrchestratorUnparseable,
        "escalation entry must record OrchestratorUnparseable kind"
    );
    assert!(
        entry.repo.as_deref() == Some(repo.0.as_str()),
        "escalation entry must carry the repo id: {:?}",
        entry.repo
    );

    let delivered = h.engine.delivered.lock().await;
    let has_directive = delivered
        .iter()
        .any(|e| matches!(e, DaemonEvent::DaemonDirective(_)));
    assert!(
        !has_directive,
        "orchestrator-dead path must not attempt a Linear write"
    );
}

/// The action-channel-closed branch (session yields `None` instead of an
/// explicit `ProcessExit`) must produce the same enqueue contract.
#[tokio::test]
async fn action_channel_close_enqueues_orchestrator_crash_entry() {
    let h = OrchHarness::new();
    // Empty stream — the next_action() call returns None on the first poll.
    h.engine.push_stream(vec![]).await;

    let issue = IssueId::from("ENG-830");
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

    let read_handle = OrchestratorReadHandle::new(h.state_map.clone(), h.escalations.clone());
    let snap = read_handle.snapshot();
    let entry = snap
        .escalations
        .iter()
        .find(|e| e.issue == issue)
        .unwrap_or_else(|| {
            panic!("OrchestratorRead snapshot must include the crash entry: {snap:?}")
        });
    assert_eq!(entry.kind, EscalationKind::OrchestratorCrash);
}
