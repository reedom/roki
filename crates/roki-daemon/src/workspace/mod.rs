//! Workspace boundary: per-`(repo, issue)` git-worktree lifecycle.
//!
//! This module implements task 6.1's locked workspace model. The
//! orchestrator depends on the [`Workspace`] trait; the default
//! [`WorkspaceManager`] resolves each repo's local checkout via
//! [`crate::tools::GhqTool`] and creates / removes per-issue worktrees via
//! [`crate::tools::WtTool`].
//!
//! ## Path layout
//!
//! For a configured repo whose ghq identifier is `owner/repo`, the local
//! checkout lives at `<ghq_root>/<host>/<owner>/<repo>` (whatever `ghq` is
//! configured to use). The worktree for a Linear issue id `ENG-42` is
//! created at `{repo_path}/../{repo_name}.{branch_sanitized}` per
//! `wt switch --create`. Branch names are the issue id verbatim; the
//! sanitizer in [`crate::tools::wt::sanitize_branch`] is the ONLY sanitizer
//! applied (characters outside `[A-Za-z0-9_-]` collapse to `-`).
//!
//! ## Cleanup
//!
//! `Cleaning` calls `wt remove` on the worktree path. The branch is NOT
//! deleted (`wt remove` preserves branches). `TerminalFailure` retains both
//! the worktree dir and the branch — the daemon simply skips the
//! `wt remove` call.
//!
//! ## Collisions
//!
//! Two distinct issue ids that sanitize to the same branch (e.g., `ENG/42`
//! and `ENG-42`) under the same repo would collide on a single worktree
//! path. The manager rejects the second `ensure` with
//! [`WorkspaceError::IdentifierCollision`] so the orchestrator never
//! silently shares a worktree across distinct keys.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::orchestrator::state::{IssueId, RepoId};
use crate::tools::wt::worktree_path_for;
use crate::tools::{GhqError, GhqTool, WtError, WtTool};

/// Operator-supplied per-repo ghq identifier (`owner/repo` or
/// `host/owner/repo`). Wrapped here so the workspace manager's signature
/// stays explicit about what it stores.
pub type GhqIdentifier = String;

/// Errors surfaced by the workspace boundary.
///
/// Every variant carries the offending path or identifier so the orchestrator
/// can satisfy Requirement 4.5 ("log the filesystem error with the offending
/// path").
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// The repository or issue identifier was rejected before any subprocess
    /// or filesystem call (e.g., empty issue id).
    #[error("invalid identifier: {reason}")]
    InvalidIdentifier { reason: String },

    /// The repo id has no entry in the operator-supplied repo index.
    #[error("repo `{repo}` is not configured (no ghq identifier registered)")]
    UnknownRepo { repo: String },

    /// Two raw identifiers sanitized to the same worktree path under the
    /// same repo. The second `ensure` is rejected so the orchestrator never
    /// silently shares a worktree between distinct Linear issues.
    #[error(
        "identifier collision: '{incoming}' would collide with existing worktree at {existing_path}"
    )]
    IdentifierCollision {
        incoming: String,
        existing_path: PathBuf,
    },

    /// `ghq` lookup or clone failed.
    #[error("ghq error for `{identifier}`: {source}")]
    Ghq {
        identifier: String,
        #[source]
        source: GhqError,
    },

    /// `wt` invocation failed (switch_create or remove).
    #[error("wt error at {path}: {source}")]
    Wt {
        path: PathBuf,
        #[source]
        source: WtError,
    },
}

/// Outbound port consumed by the orchestrator. Mirrors design.md
/// "WorkspaceManager / Service Interface". Implementations must be
/// `Send + Sync` so they can be shared across worker tasks. The signature
/// is unchanged from the pre-6.1 sandbox-dir model.
#[async_trait]
pub trait Workspace: Send + Sync {
    /// Idempotently allocate the workspace for `(repo, issue)`. Returns the
    /// worktree path the engine adapter should use as the worker CWD.
    async fn ensure(&self, repo: &RepoId, issue: &IssueId) -> Result<PathBuf, WorkspaceError>;

    /// Idempotently remove the worktree for `(repo, issue)`. Returns
    /// `Ok(())` if the worktree is already absent. Does NOT delete the
    /// underlying branch (per task 6.1 locked decision #5).
    async fn remove(&self, repo: &RepoId, issue: &IssueId) -> Result<(), WorkspaceError>;

    /// List every `(repo, issue, path)` triple the manager owns.
    ///
    /// In the worktree model, listing requires walking each configured
    /// repo's `git worktree list --porcelain`. Task 5.2 (restart recovery)
    /// owns the real implementation; the default [`WorkspaceManager`]
    /// stubs this out to an empty Vec until that wiring lands.
    async fn list_existing(&self) -> Result<Vec<(RepoId, IssueId, PathBuf)>, WorkspaceError>;
}

/// Default `Workspace` implementation backed by the `wt` and `ghq` external
/// CLIs (via the [`WtTool`] and [`GhqTool`] traits). The manager is
/// `Send + Sync` so the orchestrator can share it across worker tasks.
pub struct WorkspaceManager {
    wt: Arc<dyn WtTool>,
    ghq: Arc<dyn GhqTool>,
    /// Operator-configured map from `RepoId` to ghq identifier. Built once
    /// at bootstrap; not mutated at runtime.
    repo_index: HashMap<RepoId, GhqIdentifier>,
    /// In-memory record of every active worktree. Keyed by the worktree's
    /// absolute path; carries the raw `(RepoId, IssueId)` that produced it
    /// so collision detection can reject distinct issue ids that sanitize
    /// to the same path.
    state: Arc<Mutex<HashMap<PathBuf, TrackedWorktree>>>,
}

#[derive(Debug, Clone)]
struct TrackedWorktree {
    raw_repo: String,
    raw_issue: String,
}

impl WorkspaceManager {
    /// Construct a manager with operator-supplied tool implementations.
    pub fn new(
        wt: Arc<dyn WtTool>,
        ghq: Arc<dyn GhqTool>,
        repo_index: HashMap<RepoId, GhqIdentifier>,
    ) -> Self {
        Self {
            wt,
            ghq,
            repo_index,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn ghq_identifier(&self, repo: &RepoId) -> Result<&GhqIdentifier, WorkspaceError> {
        self.repo_index
            .get(repo)
            .ok_or_else(|| WorkspaceError::UnknownRepo {
                repo: repo.as_str().to_string(),
            })
    }
}

#[async_trait]
impl Workspace for WorkspaceManager {
    async fn ensure(&self, repo: &RepoId, issue: &IssueId) -> Result<PathBuf, WorkspaceError> {
        // Reject empty issue ids early — wt would also reject, but a typed
        // error from the workspace boundary is clearer in logs.
        if issue.as_str().is_empty() {
            return Err(WorkspaceError::InvalidIdentifier {
                reason: "issue id is empty".to_string(),
            });
        }

        let identifier = self.ghq_identifier(repo)?.clone();
        let repo_path = self
            .ghq
            .ensure_cloned(&identifier)
            .await
            .map_err(|source| WorkspaceError::Ghq {
                identifier: identifier.clone(),
                source,
            })?;

        // Pre-compute the deterministic worktree path so we can collision-
        // check before invoking `wt`. `wt switch --create` is idempotent in
        // practice (re-creating an existing worktree is a no-op + checkout),
        // but we still want to detect distinct raw ids that sanitize to
        // the same path.
        let target =
            worktree_path_for(&repo_path, issue.as_str()).map_err(|source| WorkspaceError::Wt {
                path: repo_path.clone(),
                source,
            })?;

        {
            let state = self.state.lock().await;
            if let Some(existing) = state.get(&target) {
                if existing.raw_repo != repo.as_str() || existing.raw_issue != issue.as_str() {
                    return Err(WorkspaceError::IdentifierCollision {
                        incoming: format!("{}/{}", repo.as_str(), issue.as_str()),
                        existing_path: target.clone(),
                    });
                }
                // Idempotent re-ensure with matching raw identifiers: the
                // worktree is already tracked; fall through and re-issue
                // `wt switch --create`, which is documented to be a no-op
                // when the worktree already exists.
            }
        }

        let worktree_path = self
            .wt
            .switch_create(&repo_path, issue.as_str())
            .await
            .map_err(|source| WorkspaceError::Wt {
                path: target.clone(),
                source,
            })?;

        {
            let mut state = self.state.lock().await;
            state.insert(
                worktree_path.clone(),
                TrackedWorktree {
                    raw_repo: repo.as_str().to_string(),
                    raw_issue: issue.as_str().to_string(),
                },
            );
        }

        Ok(worktree_path)
    }

    async fn remove(&self, repo: &RepoId, issue: &IssueId) -> Result<(), WorkspaceError> {
        if issue.as_str().is_empty() {
            return Err(WorkspaceError::InvalidIdentifier {
                reason: "issue id is empty".to_string(),
            });
        }

        let identifier = self.ghq_identifier(repo)?.clone();
        // For removal we use `list_path` rather than `ensure_cloned` so we
        // do not silently re-clone a repo while trying to clean it up.
        let repo_path = match self.ghq.list_path(&identifier).await {
            Ok(Some(path)) => path,
            Ok(None) => {
                // No local checkout: nothing to remove. The orchestrator's
                // collision check makes a stale tracked entry harmless;
                // drop it so the slot is reusable.
                let mut state = self.state.lock().await;
                state.retain(|_, tracked| {
                    !(tracked.raw_repo == repo.as_str() && tracked.raw_issue == issue.as_str())
                });
                return Ok(());
            }
            Err(source) => {
                return Err(WorkspaceError::Ghq { identifier, source });
            }
        };

        let worktree_path =
            worktree_path_for(&repo_path, issue.as_str()).map_err(|source| WorkspaceError::Wt {
                path: repo_path.clone(),
                source,
            })?;

        match self.wt.remove(&worktree_path).await {
            Ok(()) => {}
            Err(WtError::NonZeroExit { message }) if message_indicates_absent(&message) => {
                // The worktree was already gone. Treat as idempotent.
            }
            Err(source) => {
                return Err(WorkspaceError::Wt {
                    path: worktree_path.clone(),
                    source,
                });
            }
        }

        let mut state = self.state.lock().await;
        state.remove(&worktree_path);
        Ok(())
    }

    async fn list_existing(&self) -> Result<Vec<(RepoId, IssueId, PathBuf)>, WorkspaceError> {
        // Task 5.2 (restart recovery) owns the real implementation; the
        // worktree model requires walking `git worktree list --porcelain`
        // per configured repo, which has not been wired yet. The empty
        // Vec keeps the trait surface stable so recovery code paths
        // continue to compile against `Workspace`.
        Ok(Vec::new())
    }
}

/// Heuristic: does the captured `wt remove` stderr indicate the worktree
/// was already absent? Used to keep `remove` idempotent without parsing the
/// full `wt`/`git worktree` error taxonomy.
fn message_indicates_absent(message: &str) -> bool {
    let lowered = message.to_lowercase();
    lowered.contains("not a working tree")
        || lowered.contains("no such file or directory")
        || lowered.contains("does not exist")
}

#[cfg(test)]
mod tests {
    //! Unit tests beside the workspace boundary. These exercise the manager
    //! against hand-rolled mocks of `WtTool` and `GhqTool`, mirroring the
    //! pattern downstream integration tests use.

    use super::*;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// Records every invocation of the mock so tests can assert on the
    /// argument shape the manager produced.
    #[derive(Default)]
    struct MockWt {
        switch_create_calls: StdMutex<Vec<(PathBuf, String)>>,
        remove_calls: StdMutex<Vec<PathBuf>>,
        /// When set, the next `switch_create` returns this typed error
        /// instead of the synthetic worktree path.
        switch_create_err: StdMutex<Option<WtError>>,
    }

    #[async_trait]
    impl WtTool for MockWt {
        async fn switch_create(&self, repo_path: &Path, branch: &str) -> Result<PathBuf, WtError> {
            self.switch_create_calls
                .lock()
                .unwrap()
                .push((repo_path.to_path_buf(), branch.to_string()));
            if let Some(err) = self.switch_create_err.lock().unwrap().take() {
                return Err(err);
            }
            // Mirror the real `wt`'s convention so collision tests and
            // path-shape assertions match production.
            worktree_path_for(repo_path, branch)
        }

        async fn remove(&self, worktree_path: &Path) -> Result<(), WtError> {
            self.remove_calls
                .lock()
                .unwrap()
                .push(worktree_path.to_path_buf());
            Ok(())
        }
    }

    /// Mock that resolves a single configured identifier to a fixed path.
    struct MockGhq {
        identifier: String,
        repo_path: PathBuf,
        list_calls: StdMutex<Vec<String>>,
        ensure_calls: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl GhqTool for MockGhq {
        async fn list_path(&self, full: &str) -> Result<Option<PathBuf>, GhqError> {
            self.list_calls.lock().unwrap().push(full.to_string());
            if full == self.identifier {
                Ok(Some(self.repo_path.clone()))
            } else {
                Ok(None)
            }
        }

        async fn ensure_cloned(&self, full: &str) -> Result<PathBuf, GhqError> {
            self.ensure_calls.lock().unwrap().push(full.to_string());
            if full == self.identifier {
                Ok(self.repo_path.clone())
            } else {
                Err(GhqError::NotFoundAfterGet {
                    identifier: full.to_string(),
                })
            }
        }
    }

    fn build_manager(
        wt: Arc<MockWt>,
        ghq: Arc<MockGhq>,
        index: &[(&str, &str)],
    ) -> WorkspaceManager {
        let repo_index: HashMap<RepoId, GhqIdentifier> = index
            .iter()
            .map(|(repo, identifier)| (RepoId::new(*repo), identifier.to_string()))
            .collect();
        WorkspaceManager::new(wt, ghq, repo_index)
    }

    #[tokio::test]
    async fn ensure_resolves_repo_via_ghq_then_creates_worktree_via_wt() {
        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("scratch");
        std::fs::create_dir_all(&repo_path).unwrap();

        let wt = Arc::new(MockWt::default());
        let ghq = Arc::new(MockGhq {
            identifier: "owner/scratch".to_string(),
            repo_path: repo_path.clone(),
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        });
        let manager = build_manager(
            Arc::clone(&wt) as Arc<MockWt>,
            Arc::clone(&ghq),
            &[("scratch", "owner/scratch")],
        );

        let path = manager
            .ensure(&RepoId::new("scratch"), &IssueId::new("ENG-1"))
            .await
            .expect("ensure must succeed against the mock tools");
        // ensure_cloned was called exactly once with the configured identifier.
        assert_eq!(
            ghq.ensure_calls.lock().unwrap().clone(),
            vec!["owner/scratch".to_string()],
        );
        // wt.switch_create was called exactly once with (repo_path, branch=
        // issue id verbatim).
        let switch_calls = wt.switch_create_calls.lock().unwrap().clone();
        assert_eq!(switch_calls.len(), 1);
        assert_eq!(switch_calls[0].0, repo_path);
        assert_eq!(switch_calls[0].1, "ENG-1");
        // Returned path matches the deterministic sibling layout.
        assert_eq!(path, tmp.path().join("scratch.ENG-1"));
    }

    #[tokio::test]
    async fn ensure_rejects_unknown_repo() {
        let wt = Arc::new(MockWt::default());
        let ghq = Arc::new(MockGhq {
            identifier: "owner/scratch".to_string(),
            repo_path: PathBuf::from("/tmp/nope"),
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        });
        let manager = build_manager(wt, ghq, &[]);

        let err = manager
            .ensure(&RepoId::new("scratch"), &IssueId::new("ENG-1"))
            .await
            .expect_err("unknown repo must be rejected");
        assert!(matches!(err, WorkspaceError::UnknownRepo { .. }));
    }

    #[tokio::test]
    async fn collision_rejects_distinct_issue_ids_that_sanitize_identically() {
        // `ENG/42` and `ENG-42` both sanitize to `ENG-42` under the same
        // repo; the second ensure must be rejected.
        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("scratch");
        std::fs::create_dir_all(&repo_path).unwrap();

        let wt = Arc::new(MockWt::default());
        let ghq = Arc::new(MockGhq {
            identifier: "owner/scratch".to_string(),
            repo_path: repo_path.clone(),
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        });
        let manager = build_manager(
            Arc::clone(&wt) as Arc<MockWt>,
            ghq,
            &[("scratch", "owner/scratch")],
        );

        manager
            .ensure(&RepoId::new("scratch"), &IssueId::new("ENG-42"))
            .await
            .expect("first ensure succeeds");
        let err = manager
            .ensure(&RepoId::new("scratch"), &IssueId::new("ENG/42"))
            .await
            .expect_err("colliding ensure must be rejected");
        assert!(matches!(err, WorkspaceError::IdentifierCollision { .. }));
        // Only one switch_create invocation — the colliding one was rejected
        // before reaching `wt`.
        assert_eq!(wt.switch_create_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ensure_idempotent_for_same_raw_ids() {
        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("scratch");
        std::fs::create_dir_all(&repo_path).unwrap();

        let wt = Arc::new(MockWt::default());
        let ghq = Arc::new(MockGhq {
            identifier: "owner/scratch".to_string(),
            repo_path: repo_path.clone(),
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        });
        let manager = build_manager(
            Arc::clone(&wt) as Arc<MockWt>,
            ghq,
            &[("scratch", "owner/scratch")],
        );

        let first = manager
            .ensure(&RepoId::new("scratch"), &IssueId::new("ENG-1"))
            .await
            .unwrap();
        let second = manager
            .ensure(&RepoId::new("scratch"), &IssueId::new("ENG-1"))
            .await
            .unwrap();
        assert_eq!(first, second);
        // `wt switch --create` is called twice (the manager re-issues the
        // command on idempotent re-ensure; `wt` itself is documented as a
        // no-op when the worktree already exists). The collision table
        // tracks the path verbatim so this does not trip the collision
        // rejection.
        assert_eq!(wt.switch_create_calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn remove_calls_wt_remove_with_resolved_worktree_path() {
        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().join("scratch");
        std::fs::create_dir_all(&repo_path).unwrap();

        let wt = Arc::new(MockWt::default());
        let ghq = Arc::new(MockGhq {
            identifier: "owner/scratch".to_string(),
            repo_path: repo_path.clone(),
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        });
        let manager = build_manager(
            Arc::clone(&wt) as Arc<MockWt>,
            Arc::clone(&ghq),
            &[("scratch", "owner/scratch")],
        );

        manager
            .ensure(&RepoId::new("scratch"), &IssueId::new("ENG-1"))
            .await
            .unwrap();
        manager
            .remove(&RepoId::new("scratch"), &IssueId::new("ENG-1"))
            .await
            .expect("remove must succeed");
        let remove_calls = wt.remove_calls.lock().unwrap().clone();
        assert_eq!(remove_calls.len(), 1);
        assert_eq!(remove_calls[0], tmp.path().join("scratch.ENG-1"));
    }

    #[tokio::test]
    async fn remove_against_absent_local_checkout_is_noop() {
        // `ghq.list_path` returns None when the local clone is absent — the
        // worktree is by definition gone too. `wt.remove` must NOT be
        // invoked.
        let wt = Arc::new(MockWt::default());
        let ghq = Arc::new(MockGhq {
            identifier: "owner/never-cloned".to_string(),
            repo_path: PathBuf::from("/tmp/never"),
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        });
        let manager = build_manager(
            Arc::clone(&wt) as Arc<MockWt>,
            ghq,
            &[("scratch", "owner/configured-but-not-cloned")],
        );

        manager
            .remove(&RepoId::new("scratch"), &IssueId::new("ENG-1"))
            .await
            .expect("remove must be idempotent when the local checkout is absent");
        assert!(wt.remove_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_existing_returns_empty_until_task_5_2_lands() {
        // Documented stub: `Workspace::list_existing` returns an empty Vec
        // until task 5.2 wires `git worktree list --porcelain`.
        let wt = Arc::new(MockWt::default());
        let ghq = Arc::new(MockGhq {
            identifier: "owner/scratch".to_string(),
            repo_path: PathBuf::from("/tmp/scratch"),
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        });
        let manager = build_manager(wt, ghq, &[("scratch", "owner/scratch")]);

        let listed = manager.list_existing().await.expect("stub returns Ok");
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn ensure_rejects_empty_issue_id_with_invalid_identifier() {
        let wt = Arc::new(MockWt::default());
        let ghq = Arc::new(MockGhq {
            identifier: "owner/scratch".to_string(),
            repo_path: PathBuf::from("/tmp/scratch"),
            list_calls: StdMutex::new(Vec::new()),
            ensure_calls: StdMutex::new(Vec::new()),
        });
        let manager = build_manager(wt, ghq, &[("scratch", "owner/scratch")]);

        let err = manager
            .ensure(&RepoId::new("scratch"), &IssueId::new(""))
            .await
            .expect_err("empty issue id must be rejected");
        assert!(matches!(err, WorkspaceError::InvalidIdentifier { .. }));
    }
}
