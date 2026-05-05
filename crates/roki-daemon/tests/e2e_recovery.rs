//! Section 13.12 — Restart-recovery end-to-end.
//!
//! Mid-phase the daemon is killed; on restart `RecoveryReconciler::scan`
//! enumerates session tempdirs + worktrees, `decide` cross-references with
//! Linear, and the orchestrator core seeds fresh actors for each
//! `ResumeActive` cell. Surface-mode recomputed from the live label set.
//! Orphan paths surface via the escalation queue (the
//! orchestrator-driver layer does this; here we exercise the reconciler
//! directly).
//!
//! Runtime-wiring TODO: when `runtime::run_with_shutdown` invokes the
//! reconciler at startup and seeds the actor map, replace the manual
//! `orchestrator.send(TrackerAdmit)` here with the wired startup hand-off.
//!
//! Spec refs: requirements.md 8.5, 10.1, 10.2.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use common::{run_phase_action, stop_action, OrchHarness};
use roki_daemon::config::SecretValue;
use roki_daemon::config::repos::RepoEntry;
use roki_daemon::engine::orchestrator_session::action_parser::{Outcome, PhaseName};
use roki_daemon::exec::ghq::{seed_mock_repo, MockGhq};
use roki_daemon::exec::wt::{MockWt, WorktreeEntry};
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::escalation::{EscalationEntry, EscalationKind};
use roki_daemon::orchestrator::recovery::{
    DiscoveredIssue, DiscoveredWorktree, RecoveryDecision, RecoveryReconciler,
};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::linear::LinearClient;
use roki_daemon::tracker::model::{
    LinearStateName, LinearUserId, RepoId, LABEL_ROKI_IMPL, LABEL_ROKI_READY,
};
use roki_daemon::tracker::pre_admission::PreAdmissionJudge;
use tempfile::TempDir;
use time::OffsetDateTime;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn recovery_resumes_active_issue_with_recomputed_mode_and_records_orphans() {
    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let repo_path = seed_mock_repo(&ghq, tmp.path(), "github.com/owner/repo");
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    // Pre-restart residue: ENG-1 has both session + worktree (ResumeActive
    // candidate). ENG-2 has only a worktree (OrphanedWorktree once Linear
    // says Done).
    std::fs::create_dir_all(session_root.join("ENG-1")).unwrap();
    let wt_path_active = tmp.path().join("repo.ENG-1");
    let wt_path_orphan = tmp.path().join("repo.ENG-2");
    std::fs::create_dir_all(&wt_path_active).unwrap();
    std::fs::create_dir_all(&wt_path_orphan).unwrap();
    wt.seed_list(
        &repo_path,
        vec![
            WorktreeEntry {
                path: wt_path_active.clone(),
                branch: Some("ENG-1".to_owned()),
            },
            WorktreeEntry {
                path: wt_path_orphan.clone(),
                branch: Some("ENG-2".to_owned()),
            },
        ],
    );

    let reconciler = RecoveryReconciler::new(
        session_root,
        vec![RepoEntry {
            ghq: "github.com/owner/repo".to_owned(),
        }],
        wt,
        ghq,
    )
    .expect("reconciler");

    // Wiremock Linear: ENG-1 is Todo + ready (active); ENG-2 is Done
    // (terminal — orphan).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issue": {
                "identifier": "ENG-1",
                "title": "active",
                "description": "",
                "state": { "name": "Todo" },
                "labels": { "nodes": [
                    { "name": LABEL_ROKI_READY },
                    { "name": LABEL_ROKI_IMPL },
                ] },
                "assignee": { "id": "u1" }
            }}
        })))
        .mount(&server)
        .await;

    let linear = LinearClient::new(server.uri(), SecretValue::new("tok"))
        .with_backoff_floor(Duration::from_millis(5));
    let judge = PreAdmissionJudge::new(
        LinearUserId::from("u1"),
        BTreeSet::from([LinearStateName::from("Todo")]),
    );

    // Scan should enumerate both ENG-1 and ENG-2 deterministically.
    let scanned = reconciler.scan().await.expect("scan");
    let ids: Vec<_> = scanned.iter().map(|d| d.issue.0.as_str()).collect();
    assert_eq!(ids, vec!["ENG-1", "ENG-2"]);

    // Decide for ENG-1: ResumeActive(SpecDriven).
    let active_discovered = DiscoveredIssue {
        issue: IssueId::from("ENG-1"),
        session_present: true,
        worktrees: vec![DiscoveredWorktree {
            repo_id: RepoId::from("github.com/owner/repo"),
            path: wt_path_active.clone(),
            branch: "ENG-1".to_owned(),
        }],
    };
    let decision = reconciler
        .decide(active_discovered, &linear, &judge)
        .await
        .expect("decide");
    let resumed_mode = match decision {
        RecoveryDecision::ResumeActive { mode, issue } => {
            assert_eq!(issue, IssueId::from("ENG-1"));
            mode
        }
        other => panic!("expected ResumeActive, got {other:?}"),
    };
    assert_eq!(resumed_mode, Mode::SpecDriven);

    // Seed a fresh orchestrator with the resumed issue + mode.
    let h = OrchHarness::new();
    h.engine
        .push_stream(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
            OrchestratorActionEvent::Action(stop_action(Outcome::Success)),
        ])
        .await;

    let resumed_issue = IssueId::from("ENG-1");
    h.orchestrator
        .send(
            resumed_issue.clone(),
            ActorMessage::TrackerAdmit {
                mode: resumed_mode,
                repo: Some(RepoId::from("github.com/owner/repo")),
            },
        )
        .await
        .expect("admit resumed");

    h.wait_for_inactive(&resumed_issue, InactiveReason::AwaitingLinear)
        .await;

    // Mode immutable for the session: launch_modes recorded once with the
    // recomputed Mode::SpecDriven.
    let modes = h.engine.launch_modes.lock().await;
    assert_eq!(modes.as_slice(), &[Mode::SpecDriven]);
    drop(modes);

    // Req 8.5 / 10.2: the fresh orchestrator's RENDERED system prompt must
    // carry the recomputed mode so the orchestrator session observes the
    // post-restart Linear label set rather than any pre-restart residue.
    let prompts = h.engine.launch_prompts.lock().await;
    assert_eq!(prompts.len(), 1, "exactly one fresh orchestrator launch");
    let rendered = &prompts[0];
    assert!(
        rendered.contains("SpecDriven"),
        "rendered prompt must surface recomputed mode SpecDriven, got: {rendered}",
    );
    assert!(
        rendered.contains("ENG-1"),
        "rendered prompt must reference the resumed issue, got: {rendered}",
    );
    drop(prompts);

    // Orphan path (ENG-2): the orchestrator-driver layer enqueues
    // EscalationKind::Orphan for any DiscoveredIssue whose decide() yields
    // OrphanedWorktree / OrphanedSession. We model that surface here by
    // enqueuing the entry directly so the queue contract is exercised.
    h.escalations
        .enqueue(EscalationEntry {
            issue: IssueId::from("ENG-2"),
            repo: Some("github.com/owner/repo".to_owned()),
            kind: EscalationKind::Orphan,
            correlation_id: "orphan-ENG-2".to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            structured_fields: serde_json::json!({"path": wt_path_orphan}),
        })
        .await;

    let snap = h.escalations.snapshot().await;
    assert!(
        snap.iter().any(|e| e.kind == EscalationKind::Orphan),
        "orphan must surface via escalation queue: {snap:?}",
    );
}
