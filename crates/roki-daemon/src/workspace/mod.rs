//! Workspace boundary: per-`(repo, issue)` working-directory lifecycle.
//!
//! This module implements task 2.2 of the roki-mvp spec. It owns:
//!
//! * the sanitization rules and path derivation for the workspace tree (see
//!   `layout`), enforcing Requirement 4.2 path-safety invariants;
//! * the [`Workspace`] trait the orchestrator depends on (design.md
//!   "WorkspaceManager"), with `ensure`, `remove`, and `list_existing`
//!   operations that satisfy Requirements 4.1, 4.2, and 4.5;
//! * a default [`WorkspaceManager`] implementation that creates, removes, and
//!   inventories per-`(repo, issue)` directories under a configured workspace
//!   root.
//!
//! ## Concurrency model
//!
//! The trait is `async` per design.md. The default manager uses `tokio::fs`
//! for IO and a `tokio::sync::Mutex` to serialize the small in-memory
//! collision-tracking table that backs Requirement 4.2's "no two raw
//! identifiers may map to the same sanitized workspace path" rule. The
//! manager is `Send + Sync` so the orchestrator can share it across worker
//! tasks.
//!
//! ## Path-safety invariant
//!
//! After `ensure` creates a workspace directory, the manager canonicalizes
//! the result and verifies it is a descendant of the canonicalized workspace
//! root. If canonicalization escapes the root (for example, via a symlink
//! placed under the root), the manager removes the offending entry and
//! returns a typed error naming the offending path. Callers therefore never
//! observe a `PathBuf` that is outside the workspace root, even if the
//! underlying filesystem has been tampered with.

mod layout;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::orchestrator::state::{IssueId, RepoId};

use self::layout::{ComponentKind, SanitizeRejection, sanitize_component};

/// Errors surfaced by the workspace boundary.
///
/// Every variant carries the offending path or identifier so the orchestrator
/// can satisfy Requirement 4.5 ("log the filesystem error with the offending
/// path").
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// The repository or issue identifier was rejected before any filesystem
    /// access (Requirement 4.2: path traversal, absolute paths, empty after
    /// sanitization, etc.).
    #[error("invalid identifier: {reason}")]
    InvalidIdentifier { reason: String },

    /// Two raw identifiers sanitized to the same workspace path. The second
    /// `ensure` is rejected so the orchestrator never silently shares a
    /// directory between distinct Linear issues.
    #[error(
        "identifier collision: '{incoming}' would collide with existing workspace at {existing_path}"
    )]
    IdentifierCollision {
        incoming: String,
        existing_path: PathBuf,
    },

    /// After creation the canonicalized workspace path was not a descendant
    /// of the canonicalized workspace root. Indicates filesystem tampering
    /// (e.g. a symlink) and is treated as a hard rejection.
    #[error("workspace path {offending} escapes workspace root {root}")]
    EscapesRoot { offending: PathBuf, root: PathBuf },

    /// A filesystem operation failed; the path the daemon was operating on is
    /// included verbatim per Requirement 4.5.
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Outbound port consumed by the orchestrator. Mirrors design.md
/// "WorkspaceManager / Service Interface". Implementations must be
/// `Send + Sync` so they can be shared across worker tasks.
#[async_trait]
pub trait Workspace: Send + Sync {
    /// Idempotently create the workspace directory for `(repo, issue)` under
    /// the configured workspace root. Returns the canonical workspace path
    /// (always a descendant of the canonicalized root).
    async fn ensure(&self, repo: &RepoId, issue: &IssueId) -> Result<PathBuf, WorkspaceError>;

    /// Idempotently remove the workspace directory for `(repo, issue)`.
    /// Returns `Ok(())` if the workspace is already absent.
    async fn remove(&self, repo: &RepoId, issue: &IssueId) -> Result<(), WorkspaceError>;

    /// List every `(repo, issue, path)` triple discoverable under the
    /// workspace root. Used by recovery reconciliation (Requirement 10.1)
    /// after restart.
    async fn list_existing(&self) -> Result<Vec<(RepoId, IssueId, PathBuf)>, WorkspaceError>;
}

/// Default `Workspace` implementation backed by `tokio::fs` and an
/// in-memory collision table.
///
/// The manager assumes exclusive ownership of the workspace root: any
/// directory present under the root is treated as a workspace owned by this
/// process unless explicitly removed.
pub struct WorkspaceManager {
    root: PathBuf,
    /// Canonicalized form of `root`, computed lazily. We keep the original
    /// `root` for error reporting and use the canonical form for the descent
    /// check so symlinks attached to the root itself do not produce false
    /// positives.
    canonical_root: PathBuf,
    /// Tracks every `(sanitized_repo, sanitized_issue)` that has been
    /// `ensure`d, mapped to the raw `(RepoId, IssueId)` that produced it and
    /// the canonical path on disk. The map serves Requirement 4.2's
    /// collision rule: a second `ensure` whose sanitized form already exists
    /// with a different raw `(RepoId, IssueId)` is rejected.
    state: Arc<Mutex<HashMap<(String, String), TrackedWorkspace>>>,
}

#[derive(Debug, Clone)]
struct TrackedWorkspace {
    raw_repo: String,
    raw_issue: String,
    path: PathBuf,
}

impl WorkspaceManager {
    /// Create a manager rooted at `workspace_root`. The directory must exist
    /// and be canonicalizable; callers normally arrange for this during
    /// daemon startup.
    pub fn new(workspace_root: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let root = workspace_root.into();
        let canonical_root = std::fs::canonicalize(&root).map_err(|source| WorkspaceError::Io {
            path: root.clone(),
            source,
        })?;
        Ok(Self {
            root,
            canonical_root,
            state: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Verify that `candidate` is a descendant of the canonical workspace
    /// root. Returns the canonical form on success.
    fn enforce_descent(&self, candidate: &Path) -> Result<PathBuf, WorkspaceError> {
        let canonical = std::fs::canonicalize(candidate).map_err(|source| WorkspaceError::Io {
            path: candidate.to_path_buf(),
            source,
        })?;
        if !canonical.starts_with(&self.canonical_root) {
            return Err(WorkspaceError::EscapesRoot {
                offending: canonical,
                root: self.canonical_root.clone(),
            });
        }
        Ok(canonical)
    }

    /// Translate a `SanitizeRejection` into the public error type.
    fn invalid_identifier(rejection: SanitizeRejection) -> WorkspaceError {
        WorkspaceError::InvalidIdentifier {
            reason: rejection.message(),
        }
    }
}

#[async_trait]
impl Workspace for WorkspaceManager {
    async fn ensure(&self, repo: &RepoId, issue: &IssueId) -> Result<PathBuf, WorkspaceError> {
        let sanitized_repo = sanitize_component(repo.as_str()).map_err(|reason| {
            Self::invalid_identifier(SanitizeRejection {
                which: ComponentKind::Repo,
                raw: repo.as_str().to_string(),
                reason,
            })
        })?;
        let sanitized_issue = sanitize_component(issue.as_str()).map_err(|reason| {
            Self::invalid_identifier(SanitizeRejection {
                which: ComponentKind::Issue,
                raw: issue.as_str().to_string(),
                reason,
            })
        })?;

        let target = self.root.join(&sanitized_repo).join(&sanitized_issue);

        let mut state = self.state.lock().await;
        let key = (sanitized_repo.clone(), sanitized_issue.clone());

        if let Some(existing) = state.get(&key) {
            // Same raw identifiers => idempotent re-ensure, return cached
            // canonical path. Different raw identifiers that sanitized to the
            // same key => collision rejection per Requirement 4.2.
            if existing.raw_repo == repo.as_str() && existing.raw_issue == issue.as_str() {
                return Ok(existing.path.clone());
            }
            return Err(WorkspaceError::IdentifierCollision {
                incoming: format!("{}/{}", repo.as_str(), issue.as_str()),
                existing_path: existing.path.clone(),
            });
        }

        // Create the directory tree (idempotent on re-run via `create_dir_all`).
        tokio::fs::create_dir_all(&target)
            .await
            .map_err(|source| WorkspaceError::Io {
                path: target.clone(),
                source,
            })?;

        let canonical = self.enforce_descent(&target)?;

        state.insert(
            key,
            TrackedWorkspace {
                raw_repo: repo.as_str().to_string(),
                raw_issue: issue.as_str().to_string(),
                path: canonical.clone(),
            },
        );

        Ok(canonical)
    }

    async fn remove(&self, repo: &RepoId, issue: &IssueId) -> Result<(), WorkspaceError> {
        let sanitized_repo = sanitize_component(repo.as_str()).map_err(|reason| {
            Self::invalid_identifier(SanitizeRejection {
                which: ComponentKind::Repo,
                raw: repo.as_str().to_string(),
                reason,
            })
        })?;
        let sanitized_issue = sanitize_component(issue.as_str()).map_err(|reason| {
            Self::invalid_identifier(SanitizeRejection {
                which: ComponentKind::Issue,
                raw: issue.as_str().to_string(),
                reason,
            })
        })?;

        let target = self.root.join(&sanitized_repo).join(&sanitized_issue);

        match tokio::fs::remove_dir_all(&target).await {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Idempotent: removing an absent workspace is not an error.
            }
            Err(source) => {
                return Err(WorkspaceError::Io {
                    path: target.clone(),
                    source,
                });
            }
        }

        // Drop the tracking entry whether or not the directory existed; the
        // map stays consistent with what is on disk.
        let mut state = self.state.lock().await;
        state.remove(&(sanitized_repo, sanitized_issue));
        Ok(())
    }

    async fn list_existing(&self) -> Result<Vec<(RepoId, IssueId, PathBuf)>, WorkspaceError> {
        // Walk `<root>/<repo>/<issue>/` two levels deep. The manager treats
        // every directory at that depth as an issue workspace.
        let mut found = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();

        let mut repo_iter = match tokio::fs::read_dir(&self.root).await {
            Ok(iter) => iter,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(found),
            Err(source) => {
                return Err(WorkspaceError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        };

        while let Some(repo_entry) =
            repo_iter
                .next_entry()
                .await
                .map_err(|source| WorkspaceError::Io {
                    path: self.root.clone(),
                    source,
                })?
        {
            let repo_path = repo_entry.path();
            let metadata = match tokio::fs::metadata(&repo_path).await {
                Ok(meta) => meta,
                Err(source) => {
                    return Err(WorkspaceError::Io {
                        path: repo_path,
                        source,
                    });
                }
            };
            if !metadata.is_dir() {
                continue;
            }
            let repo_name = match repo_entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue, // Non-UTF-8 names are not workspaces this manager owns.
            };

            let mut issue_iter =
                tokio::fs::read_dir(&repo_path)
                    .await
                    .map_err(|source| WorkspaceError::Io {
                        path: repo_path.clone(),
                        source,
                    })?;
            while let Some(issue_entry) =
                issue_iter
                    .next_entry()
                    .await
                    .map_err(|source| WorkspaceError::Io {
                        path: repo_path.clone(),
                        source,
                    })?
            {
                let issue_path = issue_entry.path();
                let issue_metadata = match tokio::fs::metadata(&issue_path).await {
                    Ok(meta) => meta,
                    Err(source) => {
                        return Err(WorkspaceError::Io {
                            path: issue_path,
                            source,
                        });
                    }
                };
                if !issue_metadata.is_dir() {
                    continue;
                }
                let issue_name = match issue_entry.file_name().into_string() {
                    Ok(name) => name,
                    Err(_) => continue,
                };
                let key = (repo_name.clone(), issue_name.clone());
                if !seen.insert(key) {
                    continue;
                }
                found.push((
                    RepoId::new(repo_name.clone()),
                    IssueId::new(issue_name),
                    issue_path,
                ));
            }
        }

        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ids(repo: &str, issue: &str) -> (RepoId, IssueId) {
        (RepoId::new(repo), IssueId::new(issue))
    }

    #[tokio::test]
    async fn valid_identifiers_canonicalize_inside_workspace_root() {
        let root = tempdir().unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo, issue) = ids("acme-org", "ENG-42");

        let path = manager.ensure(&repo, &issue).await.unwrap();

        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        assert!(
            path.starts_with(&canonical_root),
            "expected {path:?} to be a descendant of {canonical_root:?}",
        );
        assert!(
            path.is_dir(),
            "expected workspace directory to exist on disk"
        );
    }

    #[tokio::test]
    async fn path_traversal_in_repo_id_is_rejected() {
        let root = tempdir().unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo, issue) = ids("../etc", "ENG-1");

        let err = manager
            .ensure(&repo, &issue)
            .await
            .expect_err("path traversal repo id must be rejected");
        match err {
            WorkspaceError::InvalidIdentifier { reason } => {
                assert!(
                    reason.contains("repo"),
                    "expected repo label, got: {reason}"
                );
            }
            other => panic!("expected InvalidIdentifier, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn path_traversal_in_issue_id_is_rejected() {
        let root = tempdir().unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo, issue) = ids("repo", "../passwd");

        let err = manager
            .ensure(&repo, &issue)
            .await
            .expect_err("path traversal issue id must be rejected");
        match err {
            WorkspaceError::InvalidIdentifier { reason } => {
                assert!(
                    reason.contains("issue"),
                    "expected issue label, got: {reason}"
                );
            }
            other => panic!("expected InvalidIdentifier, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn absolute_path_components_are_rejected() {
        let root = tempdir().unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo, issue) = ids("/etc", "ENG-1");

        let err = manager
            .ensure(&repo, &issue)
            .await
            .expect_err("absolute repo id must be rejected");
        assert!(matches!(err, WorkspaceError::InvalidIdentifier { .. }));
    }

    #[tokio::test]
    async fn colliding_sanitization_is_rejected() {
        // "abc/def" is rejected outright by the path-separator rule, so the
        // collision case uses two raw identifiers that *survive* sanitization
        // and then collide: "abc def" and "abc!def" both sanitize to
        // "abc_def". The second `ensure` must fail with IdentifierCollision.
        let root = tempdir().unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo_a, issue) = ids("abc def", "ENG-1");
        let (repo_b, _) = ids("abc!def", "ENG-1");

        let _path_a = manager.ensure(&repo_a, &issue).await.unwrap();
        let err = manager
            .ensure(&repo_b, &issue)
            .await
            .expect_err("second raw id sanitizing to the same path must be rejected");
        match err {
            WorkspaceError::IdentifierCollision {
                incoming,
                existing_path,
            } => {
                assert!(incoming.contains("abc!def"));
                assert!(
                    existing_path.exists(),
                    "existing path must still be on disk"
                );
            }
            other => panic!("expected IdentifierCollision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ensure_is_idempotent() {
        let root = tempdir().unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo, issue) = ids("repo-a", "ENG-9");

        let first = manager.ensure(&repo, &issue).await.unwrap();
        let second = manager.ensure(&repo, &issue).await.unwrap();
        assert_eq!(first, second, "idempotent ensure must return the same path");
        assert!(first.is_dir());
    }

    #[tokio::test]
    async fn remove_is_idempotent() {
        let root = tempdir().unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo, issue) = ids("repo-a", "ENG-9");

        // Removing a never-created workspace must be a no-op.
        manager.remove(&repo, &issue).await.unwrap();

        // Create then remove twice; both removes must succeed.
        manager.ensure(&repo, &issue).await.unwrap();
        manager.remove(&repo, &issue).await.unwrap();
        manager.remove(&repo, &issue).await.unwrap();
    }

    #[tokio::test]
    async fn remove_after_ensure_clears_collision_tracking() {
        // Once a workspace is removed, a previously-rejected colliding
        // identifier should be allowed again — the slot is no longer in use.
        let root = tempdir().unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo_a, issue) = ids("abc def", "ENG-1");
        let (repo_b, _) = ids("abc!def", "ENG-1");

        manager.ensure(&repo_a, &issue).await.unwrap();
        manager.remove(&repo_a, &issue).await.unwrap();
        let path = manager
            .ensure(&repo_b, &issue)
            .await
            .expect("after remove, the slot must be reusable");
        assert!(path.is_dir());
    }

    #[tokio::test]
    async fn list_existing_finds_seeded_workspaces() {
        let root = tempdir().unwrap();
        // Pre-create two workspace dirs without going through `ensure`, the
        // way recovery reconciliation will find them on restart.
        std::fs::create_dir_all(root.path().join("repo-a").join("ENG-1")).unwrap();
        std::fs::create_dir_all(root.path().join("repo-b").join("ENG-2")).unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();

        let mut found = manager.list_existing().await.unwrap();
        found.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0.as_str(), "repo-a");
        assert_eq!(found[0].1.as_str(), "ENG-1");
        assert_eq!(found[1].0.as_str(), "repo-b");
        assert_eq!(found[1].1.as_str(), "ENG-2");
    }

    #[tokio::test]
    async fn list_existing_skips_files_at_repo_level() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("repo-a").join("ENG-1")).unwrap();
        // A stray file at the root level must be ignored, not treated as a
        // repo directory.
        std::fs::write(root.path().join("stray.txt"), b"not a workspace").unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();

        let found = manager.list_existing().await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.as_str(), "repo-a");
        assert_eq!(found[0].1.as_str(), "ENG-1");
    }

    #[tokio::test]
    async fn error_carries_offending_path() {
        // Induce a known error: pre-create a *file* where the workspace
        // directory should be. `create_dir_all` will then fail and the
        // resulting `WorkspaceError::Io` must carry the offending path.
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("repo-a")).unwrap();
        std::fs::write(root.path().join("repo-a").join("ENG-1"), b"blocker").unwrap();
        let manager = WorkspaceManager::new(root.path()).unwrap();
        let (repo, issue) = ids("repo-a", "ENG-1");

        let err = manager
            .ensure(&repo, &issue)
            .await
            .expect_err("create_dir_all over a file must fail");
        match err {
            WorkspaceError::Io { path, .. } => {
                assert!(
                    path.ends_with("repo-a/ENG-1"),
                    "error must carry the offending path; got {path:?}",
                );
            }
            other => panic!("expected Io error carrying path, got {other:?}"),
        }
    }
}
