//! Integration tests for the `roki_open_worktree` agent tool (task 7.1d).
//!
//! Pinned by the task brief:
//!
//! 1. Allowlist rejection — calling the tool with a `repo` not in the
//!    operator's `[[repos]]` allowlist returns the typed
//!    `RepoNotInAllowlist` error WITHOUT invoking ghq or wt.
//! 2. Idempotency — a second call for the same `(issue, repo)` returns the
//!    existing path; ghq/wt are invoked exactly once across both calls.
//! 3. Error taxonomy — `ghq.ensure_cloned` failures surface as
//!    `GhqResolutionFailed { repo, reason }`; `wt.switch_create` failures
//!    surface as `WorktreeCreationFailed { repo, branch, reason }`.
//!
//! These mirror the unit tests beside `tools::roki_open_worktree` but
//! exercise the public surface (the `Tool` trait + `ToolError`) the
//! orchestrator wires into the agent's tool registry.

use std::sync::Arc;

use async_trait::async_trait;

mod common;

use roki_daemon::orchestrator::state::IssueId;
use roki_daemon::tools::{GhqError, GhqTool, OpenWorktreeTool, Tool, ToolError, WtError, WtTool};
use roki_daemon::worktrees::WorktreeRegistry;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;

/// Mock `GhqTool` that returns a fixed repo path or a forced error.
struct MockGhq {
    repo_path: PathBuf,
    ensure_calls: StdMutex<Vec<String>>,
    force_error: StdMutex<Option<GhqError>>,
}

#[async_trait]
impl GhqTool for MockGhq {
    async fn list_path(&self, _full: &str) -> Result<Option<PathBuf>, GhqError> {
        Ok(Some(self.repo_path.clone()))
    }

    async fn ensure_cloned(&self, full: &str) -> Result<PathBuf, GhqError> {
        self.ensure_calls.lock().unwrap().push(full.to_string());
        if let Some(err) = self.force_error.lock().unwrap().take() {
            return Err(err);
        }
        Ok(self.repo_path.clone())
    }
}

/// Mock `WtTool` that records every invocation. Materialises the worktree
/// directory so the test can assert on disk.
struct MockWt {
    switch_calls: StdMutex<Vec<(PathBuf, String)>>,
    force_error: StdMutex<Option<WtError>>,
}

#[async_trait]
impl WtTool for MockWt {
    async fn switch_create(&self, repo_path: &Path, branch: &str) -> Result<PathBuf, WtError> {
        self.switch_calls
            .lock()
            .unwrap()
            .push((repo_path.to_path_buf(), branch.to_string()));
        if let Some(err) = self.force_error.lock().unwrap().take() {
            return Err(err);
        }
        let target = roki_daemon::tools::wt::worktree_path_for(repo_path, branch)?;
        std::fs::create_dir_all(&target).map_err(|err| WtError::Io {
            message: format!("mock create_dir_all({}): {err}", target.display()),
        })?;
        Ok(target)
    }

    async fn remove(&self, _worktree_path: &Path) -> Result<(), WtError> {
        Ok(())
    }

    async fn list_porcelain(
        &self,
        _repo_path: &Path,
    ) -> Result<Vec<roki_daemon::tools::wt::WorktreePorcelainEntry>, WtError> {
        Ok(Vec::new())
    }
}

fn build(
    issue: &str,
    allowed: &[&str],
    ghq: Arc<MockGhq>,
    wt: Arc<MockWt>,
    registry: WorktreeRegistry,
) -> OpenWorktreeTool {
    OpenWorktreeTool::new(
        IssueId::new(issue),
        allowed.iter().map(|s| (*s).to_string()).collect(),
        ghq,
        wt,
        registry,
    )
}

#[tokio::test]
async fn allowlist_rejection_does_not_invoke_external_tools() {
    let parent = tempfile::TempDir::new().unwrap();
    let repo_path = parent.path().join("core");
    std::fs::create_dir_all(&repo_path).unwrap();

    let ghq = Arc::new(MockGhq {
        repo_path: repo_path.clone(),
        ensure_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(None),
    });
    let wt = Arc::new(MockWt {
        switch_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(None),
    });
    let registry = WorktreeRegistry::new();
    let tool = build(
        "ENG-99",
        &["owner/core", "owner/infra"],
        Arc::clone(&ghq),
        Arc::clone(&wt),
        registry.clone(),
    );

    let err = tool
        .call(serde_json::json!({"repo": "evil/repo"}))
        .await
        .expect_err("non-allowlisted repo must be refused");
    match err {
        ToolError::RepoNotInAllowlist { repo, allowed } => {
            assert_eq!(repo, "evil/repo");
            assert_eq!(
                allowed,
                vec!["owner/core".to_string(), "owner/infra".to_string()]
            );
        }
        other => panic!("expected RepoNotInAllowlist, got {other:?}"),
    }
    assert!(ghq.ensure_calls.lock().unwrap().is_empty());
    assert!(wt.switch_calls.lock().unwrap().is_empty());
    assert!(registry.list_for_issue(&IssueId::new("ENG-99")).is_empty());
}

#[tokio::test]
async fn idempotent_repeat_call_short_circuits_external_tools() {
    let parent = tempfile::TempDir::new().unwrap();
    let repo_path = parent.path().join("core");
    std::fs::create_dir_all(&repo_path).unwrap();

    let ghq = Arc::new(MockGhq {
        repo_path: repo_path.clone(),
        ensure_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(None),
    });
    let wt = Arc::new(MockWt {
        switch_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(None),
    });
    let registry = WorktreeRegistry::new();
    let tool = build(
        "ENG-1",
        &["owner/core"],
        Arc::clone(&ghq),
        Arc::clone(&wt),
        registry.clone(),
    );

    let first = tool
        .call(serde_json::json!({"repo": "owner/core"}))
        .await
        .expect("first call");
    let second = tool
        .call(serde_json::json!({"repo": "owner/core"}))
        .await
        .expect("second call");
    assert_eq!(first["path"], second["path"]);
    assert_eq!(ghq.ensure_calls.lock().unwrap().len(), 1);
    assert_eq!(wt.switch_calls.lock().unwrap().len(), 1);

    // Registry holds exactly one entry (issue, repo).
    let entries = registry.list_for_issue(&IssueId::new("ENG-1"));
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].repo.as_str(), "owner/core");
    assert_eq!(entries[0].branch.as_str(), "ENG-1");
}

#[tokio::test]
async fn ghq_failure_surfaces_typed_error() {
    let ghq = Arc::new(MockGhq {
        repo_path: PathBuf::from("/tmp/never"),
        ensure_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(Some(GhqError::NotFound {
            message: "ghq not on PATH".to_string(),
        })),
    });
    let wt = Arc::new(MockWt {
        switch_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(None),
    });
    let registry = WorktreeRegistry::new();
    let tool = build(
        "ENG-1",
        &["owner/core"],
        Arc::clone(&ghq),
        Arc::clone(&wt),
        registry.clone(),
    );

    let err = tool
        .call(serde_json::json!({"repo": "owner/core"}))
        .await
        .expect_err("ghq failure must surface");
    match err {
        ToolError::GhqResolutionFailed { repo, reason } => {
            assert_eq!(repo, "owner/core");
            assert!(reason.contains("ghq"));
        }
        other => panic!("expected GhqResolutionFailed, got {other:?}"),
    }
    // Failure means no registry entry written and wt was never invoked.
    assert!(wt.switch_calls.lock().unwrap().is_empty());
    assert!(registry.list_for_issue(&IssueId::new("ENG-1")).is_empty());
}

#[tokio::test]
async fn wt_failure_surfaces_typed_error() {
    let parent = tempfile::TempDir::new().unwrap();
    let repo_path = parent.path().join("core");
    std::fs::create_dir_all(&repo_path).unwrap();

    let ghq = Arc::new(MockGhq {
        repo_path: repo_path.clone(),
        ensure_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(None),
    });
    let wt = Arc::new(MockWt {
        switch_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(Some(WtError::NonZeroExit {
            message: "wt: branch already in use".to_string(),
        })),
    });
    let registry = WorktreeRegistry::new();
    let tool = build(
        "ENG-1",
        &["owner/core"],
        Arc::clone(&ghq),
        Arc::clone(&wt),
        registry.clone(),
    );

    let err = tool
        .call(serde_json::json!({"repo": "owner/core"}))
        .await
        .expect_err("wt failure must surface");
    match err {
        ToolError::WorktreeCreationFailed {
            repo,
            branch,
            reason,
        } => {
            assert_eq!(repo, "owner/core");
            assert_eq!(branch, "ENG-1");
            assert!(reason.contains("branch already in use"));
        }
        other => panic!("expected WorktreeCreationFailed, got {other:?}"),
    }
    // The registry must NOT carry an entry — the call failed before
    // registration.
    assert!(registry.list_for_issue(&IssueId::new("ENG-1")).is_empty());
}

#[tokio::test]
async fn cross_repo_calls_under_same_issue_register_distinct_entries() {
    // The agent calls `roki_open_worktree` once per repo for cross-repo
    // tickets. Each call appends a distinct entry under the same issue.
    let parent = tempfile::TempDir::new().unwrap();
    let core_path = parent.path().join("core");
    let infra_path = parent.path().join("infra");
    std::fs::create_dir_all(&core_path).unwrap();
    std::fs::create_dir_all(&infra_path).unwrap();

    // Two ghq mocks: the test issues two calls with different repos, so we
    // need a single ghq mock that resolves both via a small map. Reuse the
    // common helper from `tests/common`.
    use crate::common::{MockGhq as CommonGhq, MockWt as CommonWt};
    let ghq = Arc::new(CommonGhq::new(
        parent.path(),
        &[("owner/core", "core"), ("owner/infra", "infra")],
    ));
    let wt = Arc::new(CommonWt::default());
    let registry = WorktreeRegistry::new();
    let tool = OpenWorktreeTool::new(
        IssueId::new("ENG-7"),
        vec!["owner/core".to_string(), "owner/infra".to_string()],
        Arc::clone(&ghq) as Arc<dyn GhqTool>,
        Arc::clone(&wt) as Arc<dyn WtTool>,
        registry.clone(),
    );

    let core_out = tool
        .call(serde_json::json!({"repo": "owner/core"}))
        .await
        .unwrap();
    let infra_out = tool
        .call(serde_json::json!({"repo": "owner/infra"}))
        .await
        .unwrap();

    assert_ne!(core_out["path"], infra_out["path"]);
    let entries = registry.list_for_issue(&IssueId::new("ENG-7"));
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].repo.as_str(), "owner/core");
    assert_eq!(entries[1].repo.as_str(), "owner/infra");
}

#[tokio::test]
async fn input_schema_rejects_extra_fields() {
    let parent = tempfile::TempDir::new().unwrap();
    let repo_path = parent.path().join("core");
    std::fs::create_dir_all(&repo_path).unwrap();

    let ghq = Arc::new(MockGhq {
        repo_path,
        ensure_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(None),
    });
    let wt = Arc::new(MockWt {
        switch_calls: StdMutex::new(Vec::new()),
        force_error: StdMutex::new(None),
    });
    let registry = WorktreeRegistry::new();
    let tool = build("ENG-1", &["owner/core"], ghq, wt, registry);

    // The agent must NOT be able to override the branch name — locked
    // decision #2 hard-codes branch == issue id.
    let err = tool
        .call(serde_json::json!({"repo": "owner/core", "branch": "main"}))
        .await
        .expect_err("extra `branch` field must be refused");
    assert!(matches!(err, ToolError::InvalidInput { .. }));
}
