//! Shared test helpers for orchestrator + tool integration tests (post 7.1d).
//!
//! Provides hand-rolled mocks for `WtTool` and `GhqTool` that materialise
//! real on-disk directories so tests that probe `is_dir()` / `exists()`
//! observe the same shape they did before the worktree migration. The
//! pre-7.1d `WorkspaceManager` and `Workspace` trait are gone — tests now
//! drive `SessionManager` + `WorktreeRegistry` directly via the
//! orchestrator's new constructor surface (`Orchestrator::new(session,
//! registry, wt, ...)`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;

use roki_daemon::tools::ghq::{GhqError, GhqTool};
use roki_daemon::tools::wt::{WtError, WtTool, worktree_path_for};

/// Mock `WtTool` that materialises worktree directories on disk so tests
/// can probe `is_dir()` / `exists()`. Records every invocation so tests
/// can assert call counts and call shapes.
pub struct MockWt {
    pub switch_create_calls: StdMutex<Vec<(PathBuf, String)>>,
    pub remove_calls: StdMutex<Vec<PathBuf>>,
}

impl Default for MockWt {
    fn default() -> Self {
        Self {
            switch_create_calls: StdMutex::new(Vec::new()),
            remove_calls: StdMutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl WtTool for MockWt {
    async fn switch_create(&self, repo_path: &Path, branch: &str) -> Result<PathBuf, WtError> {
        self.switch_create_calls
            .lock()
            .unwrap()
            .push((repo_path.to_path_buf(), branch.to_string()));
        let target = worktree_path_for(repo_path, branch)?;
        std::fs::create_dir_all(&target).map_err(|err| WtError::Io {
            message: format!("mock create_dir_all({}): {err}", target.display()),
        })?;
        Ok(target)
    }

    async fn remove(&self, worktree_path: &Path) -> Result<(), WtError> {
        self.remove_calls
            .lock()
            .unwrap()
            .push(worktree_path.to_path_buf());
        if worktree_path.exists() {
            std::fs::remove_dir_all(worktree_path).map_err(|err| WtError::Io {
                message: format!("mock remove_dir_all({}): {err}", worktree_path.display()),
            })?;
        }
        Ok(())
    }
}

/// Mock `GhqTool` that resolves a fixed map of identifiers to real local
/// directories under a parent. Each repo's root directory is created when
/// the mock is built so subsequent `ensure_cloned` calls succeed without
/// touching the network.
pub struct MockGhq {
    pub roots: HashMap<String, PathBuf>,
    pub list_calls: StdMutex<Vec<String>>,
    pub ensure_calls: StdMutex<Vec<String>>,
}

impl MockGhq {
    /// Construct a new mock with one entry per `(identifier, repo_dir_name)`
    /// tuple. Repo directories are created under `parent` so the layout
    /// mimics ghq's actual behaviour.
    pub fn new(parent: &Path, entries: &[(&str, &str)]) -> Self {
        let mut roots: HashMap<String, PathBuf> = HashMap::new();
        for (identifier, dir_name) in entries {
            let repo_path = parent.join(dir_name);
            std::fs::create_dir_all(&repo_path)
                .unwrap_or_else(|err| panic!("create mock repo dir {repo_path:?}: {err}"));
            roots.insert((*identifier).to_string(), repo_path);
        }
        Self {
            roots,
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl GhqTool for MockGhq {
    async fn list_path(&self, full: &str) -> Result<Option<PathBuf>, GhqError> {
        self.list_calls.lock().unwrap().push(full.to_string());
        Ok(self.roots.get(full).cloned())
    }

    async fn ensure_cloned(&self, full: &str) -> Result<PathBuf, GhqError> {
        self.ensure_calls.lock().unwrap().push(full.to_string());
        match self.roots.get(full) {
            Some(p) => Ok(p.clone()),
            None => Err(GhqError::NotFoundAfterGet {
                identifier: full.to_string(),
            }),
        }
    }
}

/// Compute the worktree path the mock will produce for a given repo + issue.
/// Tests use this to assert "the worktree was opened at this path".
pub fn expected_worktree_path(parent: &Path, repo_dir: &str, issue: &str) -> PathBuf {
    worktree_path_for(&parent.join(repo_dir), issue).expect("worktree path")
}

/// Build a `(MockGhq, MockWt)` pair rooted under `parent` with one entry per
/// `(ghq_identifier, repo_dir_name)` tuple. Returns owned mocks the test can
/// share via `Arc`.
pub fn build_repo_mocks(parent: &Path, entries: &[(&str, &str)]) -> (Arc<MockGhq>, Arc<MockWt>) {
    let ghq = Arc::new(MockGhq::new(parent, entries));
    let wt = Arc::new(MockWt::default());
    (ghq, wt)
}

/// Build the per-test [`SessionManager`] + [`WorktreeRegistry`] wiring used
/// to construct an [`Orchestrator`]. Roots the session tempdir under
/// `session_root` (typically owned by a `tempfile::TempDir`).
pub fn build_session_wiring(
    session_root: PathBuf,
) -> (
    Arc<roki_daemon::session::SessionManager>,
    roki_daemon::worktrees::WorktreeRegistry,
) {
    let session_manager = Arc::new(roki_daemon::session::SessionManager::with_root(
        session_root,
    ));
    let registry = roki_daemon::worktrees::WorktreeRegistry::new();
    (session_manager, registry)
}

/// Construct a `WtTool` stub useful for orchestrator tests that exercise
/// the cleanup arc but do not exercise the agent tool directly. The stub
/// records `remove` invocations so tests can assert per-arc behaviour.
pub fn build_recording_wt() -> Arc<MockWt> {
    Arc::new(MockWt::default())
}
