//! Per-issue session-tempdir lifecycle (task 7.1d).
//!
//! Replaces the pre-7.1d `Workspace::ensure`/`remove` flow with a much
//! narrower concern: each in-flight Linear issue gets a dedicated session
//! tempdir under the platform cache directory. The dir becomes the worker's
//! CWD; the engine adapter starts the agent there, and the agent itself
//! decides which (if any) repos to open via the [`crate::tools::roki_open_worktree`]
//! agent tool.
//!
//! ## Path layout
//!
//! Per the design-agent-driven-repo-selection.md locked decision #5:
//!
//! * macOS: `~/Library/Caches/roki/sessions/<issue>`
//! * Linux: `~/.cache/roki/sessions/<issue>`
//!
//! Resolved at construction time via [`dirs::cache_dir`]. Tests inject a
//! deterministic root via [`SessionManager::with_root`] so each test owns a
//! `tempfile::TempDir`-backed root without polluting the host's real cache.
//!
//! ## Idempotency
//!
//! Both [`SessionManager::create_session`] and [`SessionManager::remove_session`]
//! are idempotent. `create_session` is safe to call repeatedly for the same
//! issue (returns the same path; succeeds whether or not the dir already
//! exists). `remove_session` is safe to call when the dir is already gone.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{debug, warn};

use crate::orchestrator::state::IssueId;

/// Default subdirectory under [`dirs::cache_dir`] that holds every
/// session tempdir.
const SESSIONS_SUBDIR: &str = "roki/sessions";

/// Errors surfaced by the session-tempdir lifecycle.
#[derive(Debug, Error)]
pub enum SessionError {
    /// [`dirs::cache_dir`] returned `None` and no override was supplied via
    /// [`SessionManager::with_root`]. Indicates a hostile or unusual
    /// environment (e.g., `$HOME` unset on Linux).
    #[error("cache dir is not available; set $HOME or supply an explicit root")]
    NoCacheDir,

    /// The session id was rejected before any filesystem call (empty issue).
    #[error("invalid session identifier: {reason}")]
    InvalidIdentifier { reason: String },

    /// Filesystem operation failed while creating or removing the session
    /// tempdir.
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Owns the per-issue session tempdir lifecycle.
///
/// Cheap to clone via the underlying `Arc` the orchestrator wraps it in.
/// Construction:
///
/// * [`SessionManager::new`] — production path: roots under
///   [`dirs::cache_dir`].
/// * [`SessionManager::with_root`] — test path: roots under the supplied
///   directory verbatim.
#[derive(Debug, Clone)]
pub struct SessionManager {
    root: PathBuf,
}

impl SessionManager {
    /// Construct a manager rooted at the platform's per-user cache dir
    /// (`~/Library/Caches/roki/sessions` on macOS, `~/.cache/roki/sessions`
    /// on Linux). Returns [`SessionError::NoCacheDir`] if the platform
    /// cache dir cannot be resolved.
    pub fn new() -> Result<Self, SessionError> {
        let cache = dirs::cache_dir().ok_or(SessionError::NoCacheDir)?;
        Ok(Self {
            root: cache.join(SESSIONS_SUBDIR),
        })
    }

    /// Construct a manager rooted under a caller-supplied directory. Used by
    /// tests so each test owns a `tempfile::TempDir` and never touches the
    /// real cache dir.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Root directory under which all session tempdirs live (the
    /// `<cache>/roki/sessions` directory itself).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path the session tempdir for `issue` would resolve to. Does NOT touch
    /// disk.
    pub fn session_path(&self, issue: &IssueId) -> PathBuf {
        self.root.join(issue.as_str())
    }

    /// Idempotently create the session tempdir for `issue` and return its
    /// path. Calling twice for the same issue is safe and returns the same
    /// path without error.
    pub fn create_session(&self, issue: &IssueId) -> Result<PathBuf, SessionError> {
        if issue.as_str().is_empty() {
            return Err(SessionError::InvalidIdentifier {
                reason: "issue id is empty".to_string(),
            });
        }
        let path = self.session_path(issue);
        std::fs::create_dir_all(&path).map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        debug!(
            target: "session",
            issue = %issue.as_str(),
            path = %path.display(),
            "session tempdir ensured",
        );
        Ok(path)
    }

    /// Enumerate every existing session tempdir under [`Self::root`] and
    /// return the directory names as `IssueId`s.
    ///
    /// Returns an empty vec when the sessions root does not exist (a fresh
    /// host has no `~/Library/Caches/roki/sessions/` until the daemon
    /// creates its first session). Surfacing this as an empty result keeps
    /// recovery's first-run path clean.
    ///
    /// Used by the restart-recovery walk (task 7.1e).
    pub fn list_existing_sessions(&self) -> Result<Vec<IssueId>, SessionError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let entries = std::fs::read_dir(&self.root).map_err(|source| SessionError::Io {
            path: self.root.clone(),
            source,
        })?;
        let mut issues: Vec<IssueId> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| SessionError::Io {
                path: self.root.clone(),
                source,
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            issues.push(IssueId::new(name));
        }
        // Stable lexicographic ordering so callers (recovery in particular)
        // see deterministic results across runs.
        issues.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(issues)
    }

    /// Idempotently remove the session tempdir for `issue`. Treats a missing
    /// directory as success so callers can call this from a cleanup path
    /// without checking existence first.
    pub fn remove_session(&self, issue: &IssueId) -> Result<(), SessionError> {
        if issue.as_str().is_empty() {
            return Err(SessionError::InvalidIdentifier {
                reason: "issue id is empty".to_string(),
            });
        }
        let path = self.session_path(issue);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                debug!(
                    target: "session",
                    issue = %issue.as_str(),
                    path = %path.display(),
                    "session tempdir removed",
                );
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                debug!(
                    target: "session",
                    issue = %issue.as_str(),
                    path = %path.display(),
                    "session tempdir already absent (idempotent remove)",
                );
                Ok(())
            }
            Err(err) => {
                warn!(
                    target: "session",
                    issue = %issue.as_str(),
                    path = %path.display(),
                    error = %err,
                    "session tempdir removal failed",
                );
                Err(SessionError::Io { path, source: err })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_session_is_idempotent() {
        let root = TempDir::new().expect("tempdir");
        let manager = SessionManager::with_root(root.path());
        let issue = IssueId::new("ENG-1");

        let first = manager.create_session(&issue).expect("first create");
        let second = manager.create_session(&issue).expect("second create");
        assert_eq!(first, second);
        assert!(first.is_dir());
        // Path matches the documented layout.
        assert_eq!(first, root.path().join("ENG-1"));
    }

    #[test]
    fn remove_session_is_idempotent() {
        let root = TempDir::new().expect("tempdir");
        let manager = SessionManager::with_root(root.path());
        let issue = IssueId::new("ENG-2");

        manager.create_session(&issue).expect("create");
        manager.remove_session(&issue).expect("remove existing");
        manager
            .remove_session(&issue)
            .expect("remove already-absent must succeed");
        assert!(!root.path().join("ENG-2").exists());
    }

    #[test]
    fn create_session_rejects_empty_issue() {
        let root = TempDir::new().expect("tempdir");
        let manager = SessionManager::with_root(root.path());
        let issue = IssueId::new("");
        let err = manager
            .create_session(&issue)
            .expect_err("empty id must be refused");
        assert!(matches!(err, SessionError::InvalidIdentifier { .. }));
    }

    #[test]
    fn session_path_does_not_touch_disk() {
        let root = TempDir::new().expect("tempdir");
        let manager = SessionManager::with_root(root.path());
        let issue = IssueId::new("ENG-3");
        let probe = manager.session_path(&issue);
        assert_eq!(probe, root.path().join("ENG-3"));
        assert!(!probe.exists());
    }

    #[test]
    fn list_existing_sessions_returns_empty_when_root_missing() {
        let root = TempDir::new().expect("tempdir");
        let inner = root.path().join("not-yet-created");
        let manager = SessionManager::with_root(&inner);
        let sessions = manager
            .list_existing_sessions()
            .expect("missing root must surface as empty list");
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_existing_sessions_enumerates_all_subdirs_sorted() {
        let root = TempDir::new().expect("tempdir");
        let manager = SessionManager::with_root(root.path());
        manager
            .create_session(&IssueId::new("ENG-2"))
            .expect("eng2");
        manager
            .create_session(&IssueId::new("ENG-10"))
            .expect("eng10");
        manager
            .create_session(&IssueId::new("ABC-1"))
            .expect("abc1");
        // A spurious file at the root must be ignored.
        std::fs::write(root.path().join("not-a-dir"), "").expect("write");

        let sessions = manager.list_existing_sessions().expect("list");
        let names: Vec<&str> = sessions.iter().map(|i| i.as_str()).collect();
        assert_eq!(names, vec!["ABC-1", "ENG-10", "ENG-2"]);
    }

    #[test]
    fn new_falls_back_to_platform_cache_dir() {
        // If `dirs::cache_dir()` resolves on this host, `new` succeeds and
        // points under `<cache>/roki/sessions`. Otherwise we accept the
        // documented `NoCacheDir` error. This guards against a regression
        // where `new` ever silently creates an empty path.
        match SessionManager::new() {
            Ok(manager) => {
                assert!(manager.root().ends_with("roki/sessions"));
            }
            Err(SessionError::NoCacheDir) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
