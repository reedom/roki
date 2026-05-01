//! Per-issue worktree registry (task 7.1d).
//!
//! Tracks every worktree the agent opens via the
//! [`crate::tools::roki_open_worktree`] tool, keyed by `IssueId`. The agent
//! tool consults the registry to short-circuit repeat calls (idempotency
//! guarantee #4 from `design-agent-driven-repo-selection.md`); the
//! orchestrator's `Cleaning` arc walks the registry to call `wt.remove` per
//! registered worktree.
//!
//! ## Ordering
//!
//! Per-issue insertion order is preserved so cleanup runs in the same order
//! the agent opened the worktrees, and so the per-arc cleanup logs are
//! stable across reps. Repeated `roki_open_worktree(repo)` for the SAME
//! repo within a single issue is short-circuited by the agent tool — the
//! registry merely returns the existing entry. Calls for a DIFFERENT repo
//! append a new entry.
//!
//! ## TerminalFailure retention
//!
//! Per design decision #6, worktrees are retained on `TerminalFailure`.
//! The orchestrator achieves this by simply not calling `take_for_issue`
//! on the failure arc; the registry itself has no concept of "retain" vs
//! "remove" — it just stores the records.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::orchestrator::state::{IssueId, RepoId};

/// One entry in the [`WorktreeRegistry`]: the repo the agent opened, the
/// branch (always equal to the issue id verbatim per locked decision #2),
/// and the absolute worktree path returned by `wt switch --create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredWorktree {
    pub repo: RepoId,
    pub branch: BranchName,
    pub path: PathBuf,
}

/// Newtype around the branch string. Identical to `IssueId.as_str()` today
/// but kept separate so future tasks can introduce branch-specific
/// validation without rippling through callers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchName(String);

impl BranchName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&IssueId> for BranchName {
    fn from(issue: &IssueId) -> Self {
        Self::new(issue.as_str())
    }
}

/// Inner state shared between the orchestrator (cleanup walk) and the
/// agent tool handler (lookup + insert).
type RegistryMap = HashMap<IssueId, Vec<RegisteredWorktree>>;

/// Per-issue worktree tracker.
///
/// Cheap to clone: every clone shares the same `Arc<Mutex<RegistryMap>>`.
/// All public methods are fast (in-memory map lookup or insertion); they
/// hold the mutex only long enough to read or update the inner state.
#[derive(Debug, Clone, Default)]
pub struct WorktreeRegistry {
    inner: Arc<Mutex<RegistryMap>>,
}

impl WorktreeRegistry {
    /// Construct a fresh, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up the registered worktree path for `(issue, repo)`.
    ///
    /// Returns the path of the existing worktree if the agent has already
    /// opened one for this repo on this issue. Used by the agent tool to
    /// short-circuit repeat calls before invoking ghq/wt.
    pub fn lookup(&self, issue: &IssueId, repo: &RepoId) -> Option<PathBuf> {
        let guard = self
            .inner
            .lock()
            .expect("WorktreeRegistry mutex poisoned; this is unrecoverable");
        guard
            .get(issue)
            .and_then(|entries| entries.iter().find(|e| e.repo == *repo))
            .map(|e| e.path.clone())
    }

    /// Insert a new `(repo, branch, path)` record for `issue` if no entry
    /// exists for the same repo. Returns the path that is now in the
    /// registry — either the newly-inserted one or the pre-existing one
    /// (the idempotent-second-call case).
    pub fn register(
        &self,
        issue: IssueId,
        repo: RepoId,
        branch: BranchName,
        path: PathBuf,
    ) -> PathBuf {
        let mut guard = self
            .inner
            .lock()
            .expect("WorktreeRegistry mutex poisoned; this is unrecoverable");
        let entries = guard.entry(issue).or_default();
        if let Some(existing) = entries.iter().find(|e| e.repo == repo) {
            return existing.path.clone();
        }
        entries.push(RegisteredWorktree {
            repo,
            branch,
            path: path.clone(),
        });
        path
    }

    /// Snapshot every worktree registered against `issue`, in registration
    /// (insertion) order. Returns an empty Vec when `issue` has no
    /// registered worktrees.
    pub fn list_for_issue(&self, issue: &IssueId) -> Vec<RegisteredWorktree> {
        let guard = self
            .inner
            .lock()
            .expect("WorktreeRegistry mutex poisoned; this is unrecoverable");
        guard.get(issue).cloned().unwrap_or_default()
    }

    /// Remove and return every worktree registered against `issue`.
    ///
    /// Used by the orchestrator's `Cleaning` arc to walk the registry once
    /// and surrender the entries to `wt.remove`. Returns the entries in
    /// registration (insertion) order.
    pub fn take_for_issue(&self, issue: &IssueId) -> Vec<RegisteredWorktree> {
        let mut guard = self
            .inner
            .lock()
            .expect("WorktreeRegistry mutex poisoned; this is unrecoverable");
        guard.remove(issue).unwrap_or_default()
    }
}

/// Restart-recovery surface (transitional between 7.1d and 7.1e).
///
/// 7.1e replaces this with the documented walk over both session tempdirs
/// AND every configured repo's `git worktree list --porcelain`. Until then
/// the in-memory [`WorktreeRegistry`] returns an empty Vec post-restart
/// (the registry resets on process restart) so recovery emits zero
/// `OrphanedWorktree` decisions on the production path; the test seam
/// supplies a stub returning hand-rolled `(RepoId, IssueId, PathBuf)`
/// tuples that exercise the matrix.
#[async_trait]
pub trait RecoveryListing: Send + Sync {
    /// Every `(repo, issue, worktree_path)` triple recovery should
    /// reconcile against Linear. The MVP impl returns an empty Vec; 7.1e
    /// folds the disk-walk implementation.
    async fn list_existing(&self) -> Result<Vec<(RepoId, IssueId, PathBuf)>, RecoveryListingError>;
}

/// Minimal error type for [`RecoveryListing`]. Stays narrow on purpose;
/// 7.1e expands this when it folds in `git worktree list --porcelain`
/// failure modes.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryListingError {
    #[error("recovery listing failed: {message}")]
    Other { message: String },
}

#[async_trait]
impl RecoveryListing for WorktreeRegistry {
    async fn list_existing(&self) -> Result<Vec<(RepoId, IssueId, PathBuf)>, RecoveryListingError> {
        // The registry is in-memory only; after a daemon restart it is
        // empty by construction. 7.1e replaces this with the disk-walk
        // recovery implementation.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree(repo: &str, branch: &str, path: &str) -> RegisteredWorktree {
        RegisteredWorktree {
            repo: RepoId::new(repo),
            branch: BranchName::new(branch),
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn register_short_circuits_on_repeat() {
        // Inserting the same (issue, repo) twice keeps the original entry —
        // the second call returns the original path verbatim. This is the
        // idempotency contract the agent tool depends on.
        let registry = WorktreeRegistry::new();
        let issue = IssueId::new("ENG-1");
        let repo = RepoId::new("owner/core");
        let branch = BranchName::new("ENG-1");

        let first = registry.register(
            issue.clone(),
            repo.clone(),
            branch.clone(),
            PathBuf::from("/tmp/core.ENG-1"),
        );
        let second = registry.register(
            issue.clone(),
            repo.clone(),
            branch.clone(),
            PathBuf::from("/tmp/somewhere-else"),
        );
        assert_eq!(first, PathBuf::from("/tmp/core.ENG-1"));
        assert_eq!(
            second, first,
            "second register must return the original path"
        );

        let entries = registry.list_for_issue(&issue);
        assert_eq!(entries.len(), 1, "second register must not append");
        assert_eq!(entries[0].path, PathBuf::from("/tmp/core.ENG-1"));
    }

    #[test]
    fn register_appends_for_distinct_repos() {
        // Different repos under the same issue produce distinct entries —
        // cross-repo tickets call the agent tool multiple times.
        let registry = WorktreeRegistry::new();
        let issue = IssueId::new("ENG-7");

        registry.register(
            issue.clone(),
            RepoId::new("owner/core"),
            BranchName::new("ENG-7"),
            PathBuf::from("/tmp/core.ENG-7"),
        );
        registry.register(
            issue.clone(),
            RepoId::new("owner/infra"),
            BranchName::new("ENG-7"),
            PathBuf::from("/tmp/infra.ENG-7"),
        );

        let entries = registry.list_for_issue(&issue);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].repo, RepoId::new("owner/core"));
        assert_eq!(entries[1].repo, RepoId::new("owner/infra"));
    }

    #[test]
    fn iter_for_issue_preserves_insertion_order() {
        let registry = WorktreeRegistry::new();
        let issue = IssueId::new("ENG-9");

        for (repo, path) in [
            ("owner/a", "/tmp/a.ENG-9"),
            ("owner/b", "/tmp/b.ENG-9"),
            ("owner/c", "/tmp/c.ENG-9"),
        ] {
            registry.register(
                issue.clone(),
                RepoId::new(repo),
                BranchName::new("ENG-9"),
                PathBuf::from(path),
            );
        }
        let entries = registry.list_for_issue(&issue);
        let order: Vec<&str> = entries.iter().map(|e| e.repo.as_str()).collect();
        assert_eq!(order, vec!["owner/a", "owner/b", "owner/c"]);
    }

    #[test]
    fn lookup_returns_none_for_unregistered_pair() {
        let registry = WorktreeRegistry::new();
        let issue = IssueId::new("ENG-11");
        let repo = RepoId::new("owner/core");
        assert!(registry.lookup(&issue, &repo).is_none());
    }

    #[test]
    fn take_for_issue_drains_entries_in_order() {
        let registry = WorktreeRegistry::new();
        let issue = IssueId::new("ENG-13");
        let entries = vec![
            worktree("owner/a", "ENG-13", "/tmp/a.ENG-13"),
            worktree("owner/b", "ENG-13", "/tmp/b.ENG-13"),
        ];
        for e in &entries {
            registry.register(
                issue.clone(),
                e.repo.clone(),
                e.branch.clone(),
                e.path.clone(),
            );
        }

        let taken = registry.take_for_issue(&issue);
        assert_eq!(taken, entries);
        assert!(
            registry.list_for_issue(&issue).is_empty(),
            "take_for_issue must drain the issue's entries",
        );
    }
}
