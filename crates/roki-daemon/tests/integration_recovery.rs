//! Integration smoke tests for the restart-recovery reconciler.
//!
//! Drives `RecoveryReconciler::scan` + `decide` against a wiremock-backed
//! Linear client to exercise the documented 5-cell decision matrix at the
//! public surface.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use roki_daemon::config::SecretValue;
use roki_daemon::config::repos::RepoEntry;
use roki_daemon::exec::ghq::{MockGhq, seed_mock_repo};
use roki_daemon::exec::wt::{MockWt, WorktreeEntry};
use roki_daemon::orchestrator::recovery::{
    DiscoveredIssue, DiscoveredWorktree, RecoveryDecision, RecoveryReconciler,
};
use roki_daemon::orchestrator::state::{IssueId, Mode};
use roki_daemon::tracker::linear::LinearClient;
use roki_daemon::tracker::model::{
    LABEL_ROKI_IMPL, LABEL_ROKI_READY, LinearStateName, LinearUserId, RepoId,
};
use roki_daemon::tracker::pre_admission::PreAdmissionJudge;
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn allowlist(ids: &[&str]) -> Vec<RepoEntry> {
    ids.iter()
        .map(|id| RepoEntry {
            ghq: (*id).to_owned(),
        })
        .collect()
}

fn judge_for(user: &str) -> PreAdmissionJudge {
    PreAdmissionJudge::new(
        LinearUserId::from(user),
        BTreeSet::from([LinearStateName::from("Todo")]),
    )
}

async fn linear_with_issue(
    issue_node: serde_json::Value,
) -> (MockServer, LinearClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issue": issue_node }
        })))
        .mount(&server)
        .await;
    let client = LinearClient::new(server.uri(), SecretValue::new("tok"))
        .with_backoff_floor(Duration::from_millis(5));
    (server, client)
}

fn issue_node(
    id: &str,
    state: &str,
    labels: &[&str],
    assignee: Option<&str>,
) -> serde_json::Value {
    let label_nodes: Vec<serde_json::Value> = labels
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    let assignee = match assignee {
        Some(id) => serde_json::json!({ "id": id }),
        None => serde_json::Value::Null,
    };
    serde_json::json!({
        "identifier": id,
        "title": "title",
        "description": "body",
        "state": { "name": state },
        "labels": { "nodes": label_nodes },
        "assignee": assignee,
    })
}

fn make_reconciler(
    tmp: &TempDir,
    wt: Arc<MockWt>,
    ghq: Arc<MockGhq>,
) -> (RecoveryReconciler<MockWt, MockGhq>, std::path::PathBuf) {
    let repo_path = seed_mock_repo(&ghq, tmp.path(), "github.com/owner/repo");
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
    let r = RecoveryReconciler::new(
        session_root,
        allowlist(&["github.com/owner/repo"]),
        wt,
        ghq,
    )
    .expect("reconciler");
    (r, repo_path)
}

fn touch_session(reconciler: &RecoveryReconciler<MockWt, MockGhq>, issue: &str) {
    std::fs::create_dir_all(reconciler.session_root().join(issue)).unwrap();
}

fn seed_worktree(wt: &MockWt, repo_path: &Path, worktree_path: &Path, branch: &str) {
    std::fs::create_dir_all(worktree_path).unwrap();
    wt.seed_list(
        repo_path,
        vec![WorktreeEntry {
            path: worktree_path.to_path_buf(),
            branch: Some(branch.to_owned()),
        }],
    );
}

#[tokio::test]
async fn scan_combines_session_only_and_worktree_only_findings() {
    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let (reconciler, repo_path) = make_reconciler(&tmp, wt.clone(), ghq.clone());

    touch_session(&reconciler, "ENG-1");
    let wt_path = tmp.path().join("repo.ENG-2");
    seed_worktree(&wt, &repo_path, &wt_path, "ENG-2");

    let found = reconciler.scan().await.expect("scan");
    let ids: Vec<_> = found.iter().map(|d| d.issue.0.as_str()).collect();
    assert_eq!(ids, vec!["ENG-1", "ENG-2"]);
    assert!(found[0].session_present);
    assert!(found[0].worktrees.is_empty());
    assert!(!found[1].session_present);
    assert_eq!(found[1].worktrees.len(), 1);
}

#[tokio::test]
async fn decide_resume_active_when_admission_passes_and_both_residues_present() {
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-1",
        "Todo",
        &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
        Some("u1"),
    ))
    .await;
    let judge = judge_for("u1");

    let tmp = TempDir::new().unwrap();
    let (reconciler, _repo_path) = make_reconciler(
        &tmp,
        Arc::new(MockWt::new()),
        Arc::new(MockGhq::new()),
    );

    let discovered = DiscoveredIssue {
        issue: IssueId::from("ENG-1"),
        session_present: true,
        worktrees: vec![DiscoveredWorktree {
            repo_id: RepoId::from("github.com/owner/repo"),
            path: tmp.path().join("repo.ENG-1"),
            branch: "ENG-1".to_owned(),
        }],
    };
    match reconciler.decide(discovered, &linear, &judge).await.unwrap() {
        RecoveryDecision::ResumeActive { mode, issue } => {
            assert_eq!(issue, IssueId::from("ENG-1"));
            assert_eq!(
                mode,
                Mode::SpecDriven,
                "mode must be recomputed from the live label set",
            );
        }
        other => panic!("expected ResumeActive, got {other:?}"),
    }
}

#[tokio::test]
async fn decide_resume_active_recomputes_mode_when_impl_label_dropped() {
    // Pre-restart had `roki:impl`; post-restart Linear says only `roki:ready`.
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-7",
        "Todo",
        &[LABEL_ROKI_READY],
        Some("u1"),
    ))
    .await;
    let judge = judge_for("u1");

    let tmp = TempDir::new().unwrap();
    let (reconciler, _repo_path) = make_reconciler(
        &tmp,
        Arc::new(MockWt::new()),
        Arc::new(MockGhq::new()),
    );
    let discovered = DiscoveredIssue {
        issue: IssueId::from("ENG-7"),
        session_present: true,
        worktrees: vec![DiscoveredWorktree {
            repo_id: RepoId::from("github.com/owner/repo"),
            path: tmp.path().join("repo.ENG-7"),
            branch: "ENG-7".to_owned(),
        }],
    };
    match reconciler.decide(discovered, &linear, &judge).await.unwrap() {
        RecoveryDecision::ResumeActive { mode, .. } => {
            assert_eq!(
                mode,
                Mode::NeedsClassify,
                "mode must follow the live label set, not pre-restart state",
            );
        }
        other => panic!("expected ResumeActive(NeedsClassify), got {other:?}"),
    }
}

#[tokio::test]
async fn decide_orphan_variants_route_by_which_residue_is_present() {
    // Linear says state is `Done` -> admission fails; residue still on disk.
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-2",
        "Done",
        &[LABEL_ROKI_READY],
        Some("u1"),
    ))
    .await;
    let judge = judge_for("u1");

    let tmp = TempDir::new().unwrap();
    let (reconciler, _repo_path) = make_reconciler(
        &tmp,
        Arc::new(MockWt::new()),
        Arc::new(MockGhq::new()),
    );

    // Session-only -> OrphanedSession.
    let session_only = DiscoveredIssue {
        issue: IssueId::from("ENG-2"),
        session_present: true,
        worktrees: vec![],
    };
    assert!(matches!(
        reconciler
            .decide(session_only, &linear, &judge)
            .await
            .unwrap(),
        RecoveryDecision::OrphanedSession { .. }
    ));
}

#[tokio::test]
async fn decide_fresh_queued_when_admission_passes_with_nothing_on_disk() {
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-4",
        "Todo",
        &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
        Some("u1"),
    ))
    .await;
    let judge = judge_for("u1");

    let tmp = TempDir::new().unwrap();
    let (reconciler, _repo_path) = make_reconciler(
        &tmp,
        Arc::new(MockWt::new()),
        Arc::new(MockGhq::new()),
    );

    let discovered = DiscoveredIssue {
        issue: IssueId::from("ENG-4"),
        session_present: false,
        worktrees: vec![],
    };
    match reconciler.decide(discovered, &linear, &judge).await.unwrap() {
        RecoveryDecision::FreshQueued { mode, .. } => assert_eq!(mode, Mode::SpecDriven),
        other => panic!("expected FreshQueued, got {other:?}"),
    }
}

#[tokio::test]
async fn decide_noop_when_terminal_with_nothing_on_disk() {
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-5",
        "Done",
        &[],
        Some("u1"),
    ))
    .await;
    let judge = judge_for("u1");

    let tmp = TempDir::new().unwrap();
    let (reconciler, _repo_path) = make_reconciler(
        &tmp,
        Arc::new(MockWt::new()),
        Arc::new(MockGhq::new()),
    );

    let discovered = DiscoveredIssue {
        issue: IssueId::from("ENG-5"),
        session_present: false,
        worktrees: vec![],
    };
    assert!(matches!(
        reconciler.decide(discovered, &linear, &judge).await.unwrap(),
        RecoveryDecision::NoOp { .. }
    ));
}
