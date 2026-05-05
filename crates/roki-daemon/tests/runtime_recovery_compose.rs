//! Task 10.1.3 integration test: runtime composition drives the
//! `RecoveryReconciler` scan + 5-cell decision matrix at startup, seeding
//! the orchestrator actor map with `ResumeActive` / `FreshQueued` admits and
//! the escalation queue with `Inactive(orphan)` retentions.
//!
//! Asserts the runtime composition layer (not the reconciler internals — those
//! are exercised by `tests/integration_recovery.rs`). The seam used here is
//! `runtime::testing::compose_recovery_for_test`, which mirrors the production
//! `bootstrap` step so the same code path is exercised under wiremock'd Linear
//! responses + on-disk session/worktree fixtures.
//!
//! Spec refs: requirements.md Req 8.5, 10.1, 10.2, 10.3, 10.4, 10.5;
//! design.md "Restart recovery"; "Daemon bootstrap" step 8.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use roki_daemon::config::SecretValue;
use roki_daemon::config::repos::RepoEntry;
use roki_daemon::exec::ghq::{MockGhq, seed_mock_repo};
use roki_daemon::exec::wt::{MockWt, WorktreeEntry};
use roki_daemon::orchestrator::escalation::EscalationKind;
use roki_daemon::orchestrator::recovery::RecoveryReconciler;
use roki_daemon::orchestrator::state::{IssueId, Mode};
use roki_daemon::runtime::RuntimeError;
use roki_daemon::runtime::testing::{RecoverySeedHarness, compose_recovery_for_test};
use roki_daemon::tracker::linear::LinearClient;
use roki_daemon::tracker::model::{
    LABEL_ROKI_IMPL, LABEL_ROKI_READY, LinearStateName, LinearUserId,
};
use roki_daemon::tracker::pre_admission::PreAdmissionJudge;
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Wiremock helpers
// ---------------------------------------------------------------------------

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

async fn linear_with_issue(node: serde_json::Value) -> (MockServer, Arc<LinearClient>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issue": node }
        })))
        .mount(&server)
        .await;
    let client = LinearClient::new(server.uri(), SecretValue::new("tok"))
        .with_backoff_floor(Duration::from_millis(5));
    (server, Arc::new(client))
}

fn allowlist(ids: &[&str]) -> Vec<RepoEntry> {
    ids.iter()
        .map(|id| RepoEntry { ghq: (*id).to_owned() })
        .collect()
}

fn judge_for(user: &str) -> PreAdmissionJudge {
    PreAdmissionJudge::new(
        LinearUserId::from(user),
        BTreeSet::from([LinearStateName::from("Todo")]),
    )
}

fn touch_session(session_root: &Path, issue: &str) {
    std::fs::create_dir_all(session_root.join(issue)).unwrap();
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

/// Build a `RecoveryReconciler` over `MockWt` + `MockGhq` rooted at `tmp` and
/// seed a single `[[repos]]` allowlist entry. Returns the reconciler + the
/// pre-resolved repo path so callers can seed worktrees against it.
fn build_reconciler(
    tmp: &TempDir,
    wt: Arc<MockWt>,
    ghq: Arc<MockGhq>,
) -> (RecoveryReconciler<MockWt, MockGhq>, std::path::PathBuf) {
    let repo_path = seed_mock_repo(&ghq, tmp.path(), "github.com/owner/repo");
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
    let reconciler = RecoveryReconciler::new(
        session_root,
        allowlist(&["github.com/owner/repo"]),
        wt,
        ghq,
    )
    .expect("reconciler ok");
    (reconciler, repo_path)
}

// ---------------------------------------------------------------------------
// Per-cell tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_active_cell_seeds_admit_via_inbox() {
    // Linear says ENG-1 is Todo + roki:ready + roki:impl assigned to u1.
    // Both session and worktree on disk → ResumeActive(SpecDriven).
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-1",
        "Todo",
        &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
        Some("u1"),
    ))
    .await;

    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let (reconciler, repo_path) = build_reconciler(&tmp, wt.clone(), ghq.clone());

    touch_session(reconciler.session_root(), "ENG-1");
    let wt_path = tmp.path().join("repo.ENG-1");
    seed_worktree(&wt, &repo_path, &wt_path, "ENG-1");

    let harness: RecoverySeedHarness = compose_recovery_for_test(
        reconciler,
        linear,
        judge_for("u1"),
        Duration::from_secs(10),
    )
    .await
    .expect("recovery seed ok");

    let admits = harness.admits.lock().await.clone();
    assert_eq!(
        admits.len(),
        1,
        "ResumeActive must produce exactly one Admit; got {admits:?}"
    );
    let (issue, mode, repo) = &admits[0];
    assert_eq!(issue, &IssueId::from("ENG-1"));
    assert_eq!(*mode, Mode::SpecDriven);
    assert!(repo.is_none(), "recovery seed sends Admit with repo=None");

    let escalations = harness.escalations.snapshot().await;
    assert!(
        escalations.is_empty(),
        "ResumeActive must not enqueue any escalation: {:?}",
        escalations
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_queued_cell_seeds_admit_via_inbox() {
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-4",
        "Todo",
        &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
        Some("u1"),
    ))
    .await;

    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let (reconciler, _repo_path) = build_reconciler(&tmp, wt.clone(), ghq.clone());

    // Linear says admit-passing, but discovered set is empty; we feed the
    // reconciler a synthetic empty discovery via no on-disk fixtures and
    // include ENG-4 only via the inbox seed path provided by
    // `compose_recovery_for_test` (which performs scan + decide). Because
    // nothing is on disk, scan() yields nothing — so the FreshQueued cell
    // requires a hint. Instead, seed only the session tempdir for ENG-4 to
    // force discovery; with admit-passing Linear that's still ResumeActive
    // (session present + worktree absent -> OrphanedSession).
    //
    // To exercise the FreshQueued cell through `scan + decide`, the test
    // would need a separate discovery seam; since the production scan only
    // emits issues observed on disk, FreshQueued is only reachable via the
    // tracker poller (10.1.4), not via the recovery scan. We assert that
    // the `compose_recovery_for_test` surface honours the FreshQueued branch
    // when the reconciler explicitly hands it back — exercise via direct
    // discovery injection.
    let extra_decision = harness_freshqueued_decision();
    let harness = compose_recovery_for_test_with_extra_decisions(
        reconciler,
        linear,
        judge_for("u1"),
        Duration::from_secs(10),
        vec![extra_decision],
    )
    .await
    .expect("recovery seed ok");

    let admits = harness.admits.lock().await.clone();
    assert_eq!(admits.len(), 1, "FreshQueued must produce one Admit");
    assert_eq!(admits[0].0, IssueId::from("ENG-4"));
    assert_eq!(admits[0].1, Mode::SpecDriven);
}

fn harness_freshqueued_decision() -> roki_daemon::orchestrator::recovery::RecoveryDecision {
    roki_daemon::orchestrator::recovery::RecoveryDecision::FreshQueued {
        issue: IssueId::from("ENG-4"),
        mode: Mode::SpecDriven,
    }
}

async fn compose_recovery_for_test_with_extra_decisions(
    reconciler: RecoveryReconciler<MockWt, MockGhq>,
    linear: Arc<LinearClient>,
    judge: PreAdmissionJudge,
    window: Duration,
    extra: Vec<roki_daemon::orchestrator::recovery::RecoveryDecision>,
) -> Result<RecoverySeedHarness, RuntimeError> {
    roki_daemon::runtime::testing::compose_recovery_for_test_with_extras(
        reconciler, linear, judge, window, extra,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphaned_session_cell_enqueues_orphan_escalation() {
    // Linear says ENG-2 is Done → admission fails. Session tempdir present,
    // no worktree on disk → OrphanedSession.
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-2",
        "Done",
        &[LABEL_ROKI_READY],
        Some("u1"),
    ))
    .await;

    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let (reconciler, _repo_path) = build_reconciler(&tmp, wt.clone(), ghq.clone());

    touch_session(reconciler.session_root(), "ENG-2");

    let harness = compose_recovery_for_test(
        reconciler,
        linear,
        judge_for("u1"),
        Duration::from_secs(10),
    )
    .await
    .expect("recovery seed ok");

    let admits = harness.admits.lock().await.clone();
    assert!(
        admits.is_empty(),
        "OrphanedSession must NOT produce an Admit; got {admits:?}"
    );

    let escalations = harness.escalations.snapshot().await;
    assert_eq!(escalations.len(), 1, "expected one escalation entry");
    let entry = &escalations[0];
    assert_eq!(entry.issue, IssueId::from("ENG-2"));
    assert_eq!(entry.kind, EscalationKind::Orphan);
    assert_eq!(
        entry.structured_fields["session_present"],
        serde_json::Value::Bool(true)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphaned_worktree_cell_enqueues_orphan_escalation() {
    // Linear says ENG-3 is Done; no session, worktree on disk → OrphanedWorktree.
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-3",
        "Done",
        &[],
        Some("u1"),
    ))
    .await;

    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let (reconciler, repo_path) = build_reconciler(&tmp, wt.clone(), ghq.clone());

    let wt_path = tmp.path().join("repo.ENG-3");
    seed_worktree(&wt, &repo_path, &wt_path, "ENG-3");

    let harness = compose_recovery_for_test(
        reconciler,
        linear,
        judge_for("u1"),
        Duration::from_secs(10),
    )
    .await
    .expect("recovery seed ok");

    assert!(harness.admits.lock().await.is_empty());
    let escalations = harness.escalations.snapshot().await;
    assert_eq!(escalations.len(), 1);
    let entry = &escalations[0];
    assert_eq!(entry.issue, IssueId::from("ENG-3"));
    assert_eq!(entry.kind, EscalationKind::Orphan);
    let paths = entry.structured_fields["worktree_paths"]
        .as_array()
        .expect("worktree_paths array")
        .clone();
    assert!(!paths.is_empty(), "orphan fields must carry worktree paths");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noop_cell_neither_admits_nor_escalates() {
    // Linear says terminal AND nothing on disk → NoOp.
    let (_server, linear) = linear_with_issue(issue_node(
        "ENG-5",
        "Done",
        &[],
        Some("u1"),
    ))
    .await;

    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let (reconciler, _repo_path) = build_reconciler(&tmp, wt.clone(), ghq.clone());

    // Inject an explicit NoOp decision so we exercise the `NoOp` arm of the
    // seed routine; the recovery scan does not emit anything for terminal
    // issues with no on-disk residue.
    let harness = roki_daemon::runtime::testing::compose_recovery_for_test_with_extras(
        reconciler,
        linear,
        judge_for("u1"),
        Duration::from_secs(10),
        vec![roki_daemon::orchestrator::recovery::RecoveryDecision::NoOp {
            issue: IssueId::from("ENG-5"),
        }],
    )
    .await
    .expect("recovery seed ok");

    assert!(harness.admits.lock().await.is_empty(), "NoOp must not Admit");
    assert!(
        harness.escalations.snapshot().await.is_empty(),
        "NoOp must not escalate"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_window_timeout_yields_runtime_error() {
    // Stub a wiremock that never responds; the recovery scan + decide call
    // hangs indefinitely on the per-issue `issue_by_id` lookup. The window
    // bound must surface `RuntimeError::RecoveryTimedOut` rather than
    // silently proceed.
    let server = MockServer::start().await;
    // Mount a delayed response far longer than the window so `issue_by_id`
    // hangs past the bound.
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(60))
                .set_body_json(serde_json::json!({
                    "data": { "issue": serde_json::Value::Null }
                })),
        )
        .mount(&server)
        .await;
    let linear = Arc::new(
        LinearClient::new(server.uri(), SecretValue::new("tok"))
            .with_backoff_floor(Duration::from_millis(5)),
    );

    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let (reconciler, _repo_path) = build_reconciler(&tmp, wt.clone(), ghq.clone());
    // Seed one session so `scan()` returns a non-empty discovery and the
    // reconciler attempts at least one Linear lookup which then hangs.
    touch_session(reconciler.session_root(), "ENG-9");

    let err = compose_recovery_for_test(
        reconciler,
        linear,
        judge_for("u1"),
        Duration::from_millis(150),
    )
    .await
    .expect_err("recovery must time out");
    match err {
        RuntimeError::RecoveryTimedOut { .. } => {}
        other => panic!("expected RecoveryTimedOut, got {other:?}"),
    }
}
