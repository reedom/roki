//! Shared test helpers for orchestrator + workspace integration tests.
//!
//! Provides hand-rolled mocks for `WtTool` and `GhqTool` (per task 6.1
//! locked decisions) so tests can construct a [`WorkspaceManager`] without
//! shelling out to real `wt` / `ghq` binaries. Mirrors monorail's pattern;
//! kept dependency-free (no `mockall`) so the surface stays trivial to
//! audit.
//!
//! ## Layout the mocks emulate
//!
//! Each mock owns a `tempfile::TempDir` for its repo root and lays out
//! per-issue worktrees under that root using the deterministic
//! `{repo_path}/../{repo_name}.{branch_sanitized}` rule documented by
//! [`roki_daemon::tools::wt::worktree_path_for`]. `ensure` actually
//! `mkdir`s the worktree path so existing tests that probe
//! `expected_workspace.is_dir()` keep their semantics; `remove` deletes
//! the directory so `!expected_workspace.exists()` keeps its semantics.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use tempfile::TempDir;

use roki_daemon::orchestrator::state::RepoId;
use roki_daemon::tools::ghq::{GhqError, GhqTool};
use roki_daemon::tools::wt::{WtError, WtTool, worktree_path_for};
use roki_daemon::workspace::{GhqIdentifier, WorkspaceManager};

/// Mock `WtTool` that materialises worktree directories on disk so tests
/// can probe `is_dir()` / `exists()` exactly the way they did under the
/// pre-task-6.1 sandbox-dir model.
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
        // Materialise the directory so tests that assert is_dir() see the
        // same shape as the production CLI.
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
/// directories under a `TempDir`. Each repo's root directory is created
/// when the mock is built so subsequent `ensure_cloned` calls succeed
/// without touching the network.
pub struct MockGhq {
    /// Identifier → repo path. The repo path is always inside the
    /// associated `TempDir`'s tree.
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

/// Convenience: build a `WorkspaceManager` with the mock tools rooted at
/// `parent`, plus a repo index built from the supplied
/// `(repo_id, ghq_identifier, repo_dir_name)` triples. Returns the
/// manager plus the parent `TempDir` so the test owns the lifetime.
pub fn build_workspace_manager(
    parent: TempDir,
    entries: &[(&str, &str, &str)],
) -> (WorkspaceManager, TempDir, Arc<MockWt>, Arc<MockGhq>) {
    let mock_entries: Vec<(&str, &str)> =
        entries.iter().map(|(_, ghq, dir)| (*ghq, *dir)).collect();
    let ghq = Arc::new(MockGhq::new(parent.path(), &mock_entries));
    let wt = Arc::new(MockWt::default());
    let repo_index: HashMap<RepoId, GhqIdentifier> = entries
        .iter()
        .map(|(repo_id, ghq, _)| (RepoId::new(*repo_id), (*ghq).to_string()))
        .collect();
    let manager = WorkspaceManager::new(
        Arc::clone(&wt) as Arc<dyn WtTool>,
        Arc::clone(&ghq) as Arc<dyn GhqTool>,
        repo_index,
    );
    (manager, parent, wt, ghq)
}

/// Compute the worktree path the mock will produce for a given repo +
/// issue. Tests use this to assert "the workspace is on disk" without
/// re-deriving the layout rule.
pub fn expected_worktree_path(parent: &Path, repo_dir: &str, issue: &str) -> PathBuf {
    worktree_path_for(&parent.join(repo_dir), issue).expect("worktree path")
}
