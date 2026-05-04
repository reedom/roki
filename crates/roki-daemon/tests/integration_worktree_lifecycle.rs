//! Integration smoke tests for `WorktreeManager` lifecycle.
//!
//! Drives `WorktreeManager::ensure` and `cleanup` against the public
//! `MockWt`/`MockGhq` doubles. In-file unit tests in
//! `worktree_manager::tests` already exercise individual paths; here we
//! cover the full ensure -> short-circuit -> cleanup sequence end-to-end and
//! pin the branch-scoped cleanup contract.

use std::sync::Arc;

use roki_daemon::config::repos::RepoEntry;
use roki_daemon::exec::ghq::{MockGhq, seed_mock_repo};
use roki_daemon::exec::wt::{MockWt, WorktreeEntry};
use roki_daemon::orchestrator::state::IssueId;
use roki_daemon::tracker::model::RepoId;
use roki_daemon::worktree_manager::WorktreeManager;
use tempfile::TempDir;

fn allowlist(ids: &[&str]) -> Vec<RepoEntry> {
    ids.iter()
        .map(|id| RepoEntry {
            ghq: (*id).to_owned(),
        })
        .collect()
}

#[tokio::test]
async fn ensure_first_call_creates_then_second_call_short_circuits() {
    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    seed_mock_repo(&ghq, tmp.path(), "github.com/owner/repo");

    let manager = WorktreeManager::new(
        wt.clone(),
        ghq.clone(),
        allowlist(&["github.com/owner/repo"]),
    );

    let issue = IssueId::from("ENG-42");
    let repo = RepoId::from("github.com/owner/repo");

    // First call: invokes ghq.list_path + wt.switch_create exactly once.
    let path1 = manager.ensure(&issue, &repo).await.expect("first ensure");
    assert!(path1.exists());
    assert_eq!(wt.switch_create_calls().len(), 1);
    let ghq_calls_after_first = ghq.list_calls().len();
    assert!(
        ghq_calls_after_first >= 1,
        "ghq.list_path must be invoked at least once on first call",
    );

    // Second call: must observe the seeded entry from switch_create and
    // short-circuit without a second switch_create invocation.
    let path2 = manager.ensure(&issue, &repo).await.expect("second ensure");
    assert_eq!(path1, path2);
    assert_eq!(
        wt.switch_create_calls().len(),
        1,
        "second ensure must short-circuit via list_porcelain",
    );
}

#[tokio::test]
async fn cleanup_only_removes_worktrees_whose_branch_matches_issue_id() {
    let tmp = TempDir::new().unwrap();
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let repo_path = seed_mock_repo(&ghq, tmp.path(), "github.com/owner/repo");

    let target = tmp.path().join("repo.ENG-42");
    let unrelated = tmp.path().join("repo.ENG-99");
    let detached = tmp.path().join("repo.detached");
    for p in [&target, &unrelated, &detached] {
        std::fs::create_dir_all(p).unwrap();
    }
    wt.seed_list(
        &repo_path,
        vec![
            WorktreeEntry {
                path: target.clone(),
                branch: Some("ENG-42".to_owned()),
            },
            WorktreeEntry {
                path: unrelated.clone(),
                branch: Some("ENG-99".to_owned()),
            },
            WorktreeEntry {
                path: detached.clone(),
                branch: None,
            },
        ],
    );

    let manager = WorktreeManager::new(
        wt.clone(),
        ghq.clone(),
        allowlist(&["github.com/owner/repo"]),
    );

    let removed = manager
        .cleanup(&IssueId::from("ENG-42"))
        .await
        .expect("cleanup");
    assert_eq!(removed, vec![target.clone()]);
    assert_eq!(wt.remove_calls(), vec![target]);
    assert!(unrelated.exists(), "unrelated branch survives cleanup");
    assert!(detached.exists(), "detached worktree survives cleanup");
}

#[tokio::test]
async fn ensure_rejects_repos_outside_allowlist() {
    let wt = Arc::new(MockWt::new());
    let ghq = Arc::new(MockGhq::new());
    let manager = WorktreeManager::new(
        wt,
        ghq,
        allowlist(&["github.com/owner/allowed"]),
    );
    let err = manager
        .ensure(
            &IssueId::from("ENG-1"),
            &RepoId::from("github.com/foo/bar"),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("github.com/foo/bar"),
        "AllowlistRejected error must surface the offending repo id: {err}",
    );
}
