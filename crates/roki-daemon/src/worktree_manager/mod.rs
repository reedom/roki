//! Per-issue worktree lifecycle: idempotent ensure, branch-scoped cleanup,
//! and allowlist enforcement.
//!
//! Boundary: DAEMON-INTERNAL. Driven by [`crate::exec::wt::WtTool`] and
//! [`crate::exec::ghq::GhqTool`] adapters. Phase subprocesses must not
//! depend on this module — they reach git via their own Bash tool.
//!
//! Spec refs: requirements.md Req 4.5, 4.6, 4.9, 10.1, 10.2.

pub mod allowlist;

use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

use async_trait::async_trait;

use crate::config::repos::RepoEntry;
use crate::exec::ghq::{GhqError, GhqTool};
use crate::exec::wt::{WorktreeEntry, WtError, WtTool};
use crate::orchestrator::core::{WorktreeOpError, WorktreeOps};
use crate::orchestrator::state::IssueId;
use crate::session::{SessionError, sanitize_issue_id};
use crate::tracker::model::RepoId;

#[derive(Debug, Error)]
pub enum WorktreeError {
    /// Repo identifier is not in `[[repos]]`. Maps to
    /// `Inactive(allowlist_rejected)` at the orchestrator level.
    #[error("repo `{0}` is not in the [[repos]] allowlist")]
    AllowlistRejected(String),

    /// Issue identifier failed sanitization (invalid chars, traversal).
    #[error(transparent)]
    InvalidIssueId(SessionError),

    /// Backing filesystem op failed; surfaced so the orchestrator can map to
    /// `Inactive(fs_poison)`.
    #[error("filesystem error: {0}")]
    FsPoison(String),

    /// Wrapped `wt`/`git worktree` failure.
    #[error(transparent)]
    Wt(#[from] WtError),

    /// Wrapped `ghq` failure.
    #[error(transparent)]
    Ghq(#[from] GhqError),
}

/// Idempotent per-issue worktree lifecycle manager.
///
/// `WtTool`/`GhqTool` are taken by `Arc` so the same handles can be shared
/// across the orchestrator and other daemon subsystems without forcing
/// cloning of the underlying drivers.
#[derive(Debug)]
pub struct WorktreeManager<W: WtTool, G: GhqTool> {
    wt: Arc<W>,
    ghq: Arc<G>,
    allowlist: Vec<RepoEntry>,
}

impl<W: WtTool, G: GhqTool> WorktreeManager<W, G> {
    pub fn new(wt: Arc<W>, ghq: Arc<G>, allowlist: Vec<RepoEntry>) -> Self {
        Self { wt, ghq, allowlist }
    }

    pub fn allowlist(&self) -> &[RepoEntry] {
        &self.allowlist
    }

    /// Refuse repo identifiers that are not in `[[repos]]`. Surfaced as a
    /// dedicated error variant so the orchestrator's outcome maps cleanly to
    /// `Inactive(allowlist_rejected)` per Req 4.5.
    pub fn validate_repo_in_allowlist(
        &self,
        repo_id: &RepoId,
    ) -> Result<(), WorktreeError> {
        if allowlist::is_allowed(&self.allowlist, repo_id) {
            Ok(())
        } else {
            Err(WorktreeError::AllowlistRejected(repo_id.0.clone()))
        }
    }

    /// Ensure a worktree exists for `(issue, repo)`. First call invokes
    /// `ghq.list_path` + `wt.switch_create`; subsequent calls short-circuit
    /// once `wt.list_porcelain` reports an existing entry whose branch name
    /// equals the issue id verbatim.
    pub async fn ensure(
        &self,
        issue: &IssueId,
        repo_id: &RepoId,
    ) -> Result<PathBuf, WorktreeError> {
        self.validate_repo_in_allowlist(repo_id)?;
        sanitize_issue_id(&issue.0).map_err(WorktreeError::InvalidIssueId)?;

        let repo_path = self.resolve_repo_path(repo_id).await?;

        // Short-circuit: existing worktree for this branch is reused.
        let existing = self.wt.list_porcelain(&repo_path).await?;
        if let Some(entry) = existing.iter().find(|e| matches_issue_branch(e, issue)) {
            return Ok(entry.path.clone());
        }

        let path = self.wt.switch_create(&repo_path, &issue.0).await?;
        Ok(path)
    }

    /// Tear down every worktree across the allowlist whose branch matches
    /// `issue` verbatim. Branches are NOT deleted (Req 4.9). Returns the list
    /// of removed paths.
    pub async fn cleanup(&self, issue: &IssueId) -> Result<Vec<PathBuf>, WorktreeError> {
        sanitize_issue_id(&issue.0).map_err(WorktreeError::InvalidIssueId)?;

        let mut removed = Vec::new();
        for entry in &self.allowlist {
            let repo_id = RepoId(entry.ghq.clone());
            // A repo present in the allowlist may not yet be cloned locally.
            // Treat that as "no worktrees to clean" rather than an error.
            let Some(repo_path) = self.ghq.list_path(&repo_id.0).await? else {
                continue;
            };
            let entries = match self.wt.list_porcelain(&repo_path).await {
                Ok(e) => e,
                Err(err) => return Err(WorktreeError::Wt(err)),
            };
            for wt_entry in entries.into_iter().filter(|e| matches_issue_branch(e, issue)) {
                self.wt.remove(&wt_entry.path).await?;
                removed.push(wt_entry.path);
            }
        }
        Ok(removed)
    }

    async fn resolve_repo_path(&self, repo_id: &RepoId) -> Result<PathBuf, WorktreeError> {
        // The daemon assumes the operator has already cloned listed repos;
        // surface a typed error rather than auto-cloning so a missing repo
        // is operator-visible.
        match self.ghq.list_path(&repo_id.0).await? {
            Some(path) if path.exists() => Ok(path),
            Some(path) => Err(WorktreeError::FsPoison(format!(
                "ghq returned `{}` but the path is missing on disk",
                path.display()
            ))),
            None => Err(WorktreeError::FsPoison(format!(
                "repo `{}` is in [[repos]] but not cloned via ghq",
                repo_id.0
            ))),
        }
    }
}

fn matches_issue_branch(entry: &WorktreeEntry, issue: &IssueId) -> bool {
    entry.branch.as_deref() == Some(issue.0.as_str())
}

/// Adapt [`WorktreeManager`] to the orchestrator core's [`WorktreeOps`]
/// seam. Pure routing — every call delegates to the existing
/// `WorktreeManager` method. The error variants are mapped onto the
/// orchestrator's documented taxonomy:
/// - [`WorktreeError::AllowlistRejected`] -> [`WorktreeOpError::AllowlistRejected`]
///   (orchestrator stops with `Inactive(allowlist_rejected)`).
/// - [`WorktreeError::FsPoison`] -> [`WorktreeOpError::FsPoison`]
///   (orchestrator routes through `handle_fs_poison`).
/// - Any other backing failure (`InvalidIssueId`, `Wt`, `Ghq`) -> [`WorktreeOpError::Other`].
///
/// Behavior of `WorktreeManager` itself is unchanged.
#[async_trait]
impl<W, G> WorktreeOps for WorktreeManager<W, G>
where
    W: WtTool + Send + Sync + 'static,
    G: GhqTool + Send + Sync + 'static,
{
    async fn ensure(
        &self,
        issue: &IssueId,
        repo_id: &RepoId,
    ) -> Result<PathBuf, WorktreeOpError> {
        WorktreeManager::ensure(self, issue, repo_id)
            .await
            .map_err(map_worktree_error)
    }

    async fn cleanup(&self, issue: &IssueId) -> Result<Vec<PathBuf>, WorktreeOpError> {
        WorktreeManager::cleanup(self, issue)
            .await
            .map_err(map_worktree_error)
    }
}

fn map_worktree_error(err: WorktreeError) -> WorktreeOpError {
    match err {
        WorktreeError::AllowlistRejected(repo) => WorktreeOpError::AllowlistRejected(repo),
        WorktreeError::FsPoison(cause) => WorktreeOpError::FsPoison(cause),
        other => WorktreeOpError::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ghq::{MockGhq, seed_mock_repo};
    use crate::exec::wt::{MockWt, WorktreeEntry};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn allowlist(ids: &[&str]) -> Vec<RepoEntry> {
        ids.iter()
            .map(|id| RepoEntry { ghq: (*id).to_owned() })
            .collect()
    }

    #[tokio::test]
    async fn ensure_first_call_invokes_list_then_switch_create() {
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

        let path = manager.ensure(&issue, &repo).await.unwrap();
        assert!(path.exists());
        assert_eq!(wt.switch_create_calls().len(), 1);
        assert!(path.to_string_lossy().contains("ENG-42"));
    }

    #[tokio::test]
    async fn ensure_second_call_short_circuits() {
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

        let first = manager.ensure(&issue, &repo).await.unwrap();
        let second = manager.ensure(&issue, &repo).await.unwrap();
        assert_eq!(first, second);
        // Only the initial call may invoke switch_create — the rerun must
        // observe the existing entry seeded by the first invocation.
        assert_eq!(wt.switch_create_calls().len(), 1);
    }

    #[tokio::test]
    async fn cleanup_only_removes_matching_branch() {
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let repo_path = seed_mock_repo(&ghq, tmp.path(), "github.com/owner/repo");

        // Pre-seed two entries: one for ENG-42 and one for ENG-99.
        let target_path = tmp.path().join("repo.ENG-42");
        let other_path = tmp.path().join("repo.ENG-99");
        std::fs::create_dir_all(&target_path).unwrap();
        std::fs::create_dir_all(&other_path).unwrap();
        wt.seed_list(
            &repo_path,
            vec![
                WorktreeEntry {
                    path: target_path.clone(),
                    branch: Some("ENG-42".to_owned()),
                },
                WorktreeEntry {
                    path: other_path.clone(),
                    branch: Some("ENG-99".to_owned()),
                },
            ],
        );

        let manager = WorktreeManager::new(
            wt.clone(),
            ghq.clone(),
            allowlist(&["github.com/owner/repo"]),
        );

        let removed = manager.cleanup(&IssueId::from("ENG-42")).await.unwrap();
        assert_eq!(removed, vec![target_path.clone()]);
        assert_eq!(wt.remove_calls(), vec![target_path]);
        assert!(other_path.exists(), "unrelated worktrees survive cleanup");
    }

    #[tokio::test]
    async fn out_of_allowlist_is_rejected() {
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let manager = WorktreeManager::new(
            wt,
            ghq,
            allowlist(&["github.com/owner/allowed"]),
        );
        let err = manager
            .ensure(&IssueId::from("ENG-1"), &RepoId::from("github.com/foo/bar"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, WorktreeError::AllowlistRejected(ref s) if s == "github.com/foo/bar"),
            "expected AllowlistRejected, got {err:?}",
        );
    }

    #[tokio::test]
    async fn cleanup_tolerates_extra_unrelated_entries() {
        // Recovery scan + agent-created worktrees may leave entries with
        // arbitrary branch names; cleanup must walk past them.
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let repo_path = seed_mock_repo(&ghq, tmp.path(), "github.com/owner/repo");

        let target = tmp.path().join("repo.ENG-42");
        let unrelated = tmp.path().join("repo.scratch");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        wt.seed_list(
            &repo_path,
            vec![
                WorktreeEntry {
                    path: target.clone(),
                    branch: Some("ENG-42".to_owned()),
                },
                WorktreeEntry {
                    path: unrelated.clone(),
                    branch: Some("scratch".to_owned()),
                },
                WorktreeEntry {
                    path: tmp.path().join("repo.detached"),
                    branch: None,
                },
            ],
        );

        let manager = WorktreeManager::new(
            wt.clone(),
            ghq.clone(),
            allowlist(&["github.com/owner/repo"]),
        );
        let removed = manager.cleanup(&IssueId::from("ENG-42")).await.unwrap();
        assert_eq!(removed, vec![target]);
        assert!(unrelated.exists());
    }

    #[tokio::test]
    async fn ensure_rejects_invalid_issue_id() {
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let manager = WorktreeManager::new(
            wt,
            ghq,
            allowlist(&["github.com/owner/repo"]),
        );
        let err = manager
            .ensure(
                &IssueId::from("../escape"),
                &RepoId::from("github.com/owner/repo"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::InvalidIssueId(_)));
    }
}
