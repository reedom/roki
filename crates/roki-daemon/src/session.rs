//! Per-issue ephemeral session tempdir lifecycle.
//!
//! The session manager owns `<root>/<issue>` directories used as scratch
//! space by orchestrator and phase subprocesses. Construction, idempotent
//! re-entry, and removal go through this module so the path-traversal /
//! sanitization rules live in one place.
//!
//! Spec refs: requirements.md Req 4.6, 4.8, 4.11, 10.5.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use thiserror::Error;

use crate::orchestrator::core::{SessionDirError as CoreSessionDirError, SessionDirOps};
use crate::orchestrator::state::IssueId;

/// Default platform cache root: `~/Library/Caches/roki/sessions` on macOS,
/// `~/.cache/roki/sessions` on Linux, falling back to a relative path under
/// the current user's home if the platform cache directory is unavailable.
fn default_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_default()
        .join("roki/sessions")
}

/// Manages the lifecycle of `<root>/<issue>` ephemeral directories.
///
/// The manager is `Send + Sync`: a Mutex-guarded reservation table tracks
/// active sessions to refuse cross-issue identifier collisions after
/// sanitization.
#[derive(Debug)]
pub struct SessionManager {
    root: PathBuf,
    /// Maps the sanitized identifier back to the originating raw issue id so
    /// two distinct issue identifiers that sanitize to the same on-disk path
    /// cannot share a session directory.
    reservations: Mutex<HashMap<String, String>>,
}

/// Errors surfaced by [`SessionManager`] and [`sanitize_issue_id`].
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("issue identifier `{id}` rejected: {reason}")]
    InvalidIssueId { id: String, reason: String },

    #[error(
        "issue identifier `{incoming}` collides with already-active session for \
         `{existing}` after sanitization"
    )]
    Collision { incoming: String, existing: String },

    #[error("filesystem error operating on `{path}`: {source}")]
    Filesystem {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "sanitized path `{candidate}` escapes the configured session root \
         `{root}`"
    )]
    PathEscape { candidate: PathBuf, root: PathBuf },
}

impl SessionManager {
    /// Construct with the default platform cache root. Callers who need a
    /// caller-supplied root (tests, future operator override) use
    /// [`Self::with_root`].
    pub fn new() -> Self {
        Self::with_root(default_root())
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            reservations: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Idempotent ensure of `<root>/<issue>`. Re-entrant for the same issue
    /// id; rejects cross-issue collisions after sanitization.
    pub fn ensure(&self, issue: &IssueId) -> Result<PathBuf, SessionError> {
        let raw = issue.0.as_str();
        let sanitized = sanitize_issue_id(raw)?;
        let path = self.resolve(&sanitized)?;
        self.reserve(&sanitized, raw)?;
        if !path.exists() {
            std::fs::create_dir_all(&path).map_err(|err| SessionError::Filesystem {
                path: path.clone(),
                source: err,
            })?;
        }
        Ok(path)
    }

    /// Remove `<root>/<issue>` recursively if present. Idempotent: missing
    /// directories are not an error.
    pub fn remove(&self, issue: &IssueId) -> Result<(), SessionError> {
        let raw = issue.0.as_str();
        let sanitized = sanitize_issue_id(raw)?;
        let path = self.resolve(&sanitized)?;
        if path.exists() {
            std::fs::remove_dir_all(&path).map_err(|err| SessionError::Filesystem {
                path: path.clone(),
                source: err,
            })?;
        }
        // Drop the reservation once on-disk state is gone so a subsequent
        // ensure with the same id starts fresh.
        if let Ok(mut table) = self.reservations.lock() {
            table.remove(&sanitized);
        }
        Ok(())
    }

    pub fn exists(&self, issue: &IssueId) -> bool {
        let Ok(sanitized) = sanitize_issue_id(issue.0.as_str()) else {
            return false;
        };
        let Ok(path) = self.resolve(&sanitized) else {
            return false;
        };
        path.exists()
    }

    fn resolve(&self, sanitized: &str) -> Result<PathBuf, SessionError> {
        let candidate = self.root.join(sanitized);
        // Defensive: ensure candidate is still under root after join. With a
        // sanitized identifier this is invariant, but the explicit check makes
        // the contract obvious to readers.
        if !candidate.starts_with(&self.root) {
            return Err(SessionError::PathEscape {
                candidate,
                root: self.root.clone(),
            });
        }
        Ok(candidate)
    }

    fn reserve(&self, sanitized: &str, raw: &str) -> Result<(), SessionError> {
        let mut table = self
            .reservations
            .lock()
            .expect("session reservation mutex poisoned");
        match table.get(sanitized) {
            Some(existing) if existing == raw => Ok(()),
            Some(existing) => Err(SessionError::Collision {
                incoming: raw.to_owned(),
                existing: existing.clone(),
            }),
            None => {
                table.insert(sanitized.to_owned(), raw.to_owned());
                Ok(())
            }
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Adapt the concrete [`SessionManager`] to the orchestrator core's
/// [`SessionDirOps`] seam. Pure routing — every call delegates to the
/// matching `SessionManager` method and re-tags the error string under
/// `SessionDirError::Other` so the orchestrator's `fs_poison` taxonomy stays
/// the only path that surfaces session-tempdir failures (Req 8.1
/// `Inactive(fs_poison)`). Behavior of `SessionManager` itself is unchanged.
impl SessionDirOps for SessionManager {
    fn ensure(&self, issue: &IssueId) -> Result<std::path::PathBuf, CoreSessionDirError> {
        SessionManager::ensure(self, issue).map_err(|err| CoreSessionDirError::Other(err.to_string()))
    }

    fn remove(&self, issue: &IssueId) -> Result<(), CoreSessionDirError> {
        SessionManager::remove(self, issue).map_err(|err| CoreSessionDirError::Other(err.to_string()))
    }
}

/// Validate a Linear-style issue identifier and return the sanitized form
/// safe to use as a single path segment. Canonical Linear ids match
/// `^[A-Z]+-\d+$`; the only additional characters tolerated for
/// future-proofing are ASCII alphanumerics, `_`, and `-`.
pub fn sanitize_issue_id(id: &str) -> Result<String, SessionError> {
    if id.is_empty() {
        return Err(SessionError::InvalidIssueId {
            id: id.to_owned(),
            reason: "identifier is empty".to_owned(),
        });
    }

    // Reject path-traversal hints up front to make the failure message
    // operator-actionable rather than burying the cause in a character check.
    if id.contains("..") {
        return Err(SessionError::InvalidIssueId {
            id: id.to_owned(),
            reason: "path-traversal segment `..` is not allowed".to_owned(),
        });
    }
    if id.starts_with('/') || id.starts_with('\\') {
        return Err(SessionError::InvalidIssueId {
            id: id.to_owned(),
            reason: "absolute paths are not allowed".to_owned(),
        });
    }
    if id.contains('/') || id.contains('\\') {
        return Err(SessionError::InvalidIssueId {
            id: id.to_owned(),
            reason: "path separators are not allowed".to_owned(),
        });
    }
    if id.contains('\0') {
        return Err(SessionError::InvalidIssueId {
            id: id.to_owned(),
            reason: "NUL byte is not allowed".to_owned(),
        });
    }

    for ch in id.chars() {
        let allowed = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_';
        if !allowed {
            return Err(SessionError::InvalidIssueId {
                id: id.to_owned(),
                reason: format!("character `{ch}` is outside [A-Za-z0-9_-]"),
            });
        }
    }

    Ok(id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manager() -> (SessionManager, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let mgr = SessionManager::with_root(dir.path().to_path_buf());
        (mgr, dir)
    }

    #[test]
    fn ensure_creates_directory_under_root() {
        let (mgr, _dir) = manager();
        let issue = IssueId::from("ENG-42");
        let path = mgr.ensure(&issue).expect("ensure must succeed");
        assert!(path.exists());
        assert_eq!(path.parent(), Some(mgr.root()));
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "ENG-42");
    }

    #[test]
    fn ensure_is_idempotent_for_same_issue() {
        let (mgr, _dir) = manager();
        let issue = IssueId::from("ENG-42");
        let first = mgr.ensure(&issue).unwrap();
        let second = mgr.ensure(&issue).unwrap();
        assert_eq!(first, second);
        assert!(mgr.exists(&issue));
    }

    #[test]
    fn remove_cleans_up_directory_and_clears_reservation() {
        let (mgr, _dir) = manager();
        let issue = IssueId::from("ROKI-1");
        let path = mgr.ensure(&issue).unwrap();
        assert!(path.exists());
        mgr.remove(&issue).unwrap();
        assert!(!path.exists());
        // Re-ensure must succeed after remove.
        let again = mgr.ensure(&issue).unwrap();
        assert!(again.exists());
    }

    #[test]
    fn remove_missing_directory_is_not_an_error() {
        let (mgr, _dir) = manager();
        mgr.remove(&IssueId::from("ENG-999")).unwrap();
    }

    #[test]
    fn traversal_attempts_are_rejected() {
        for hostile in [
            "../foo",
            "..",
            "a/../b",
            "/abs/path",
            "\\abs\\path",
            "ENG/42",
            "",
            "ENG 42",
            "ENG;42",
            "ENG\0NUL", // embedded NUL byte
        ] {
            let err = sanitize_issue_id(hostile).unwrap_err();
            assert!(
                matches!(err, SessionError::InvalidIssueId { .. }),
                "expected InvalidIssueId for `{hostile}`, got {err:?}",
            );
        }
    }

    #[test]
    fn canonical_linear_ids_accepted() {
        for id in ["ENG-42", "ROKI-1", "ABC-9999"] {
            let sanitized = sanitize_issue_id(id).unwrap();
            assert_eq!(sanitized, id);
        }
    }

    #[test]
    fn ensure_rejects_traversal_via_issue_id() {
        let (mgr, _dir) = manager();
        let err = mgr.ensure(&IssueId::from("../escape")).unwrap_err();
        assert!(matches!(err, SessionError::InvalidIssueId { .. }));
    }

    #[test]
    fn cross_issue_collision_is_rejected_after_sanitization() {
        // Both ids sanitize to the same on-disk segment (`ENG-42`) since the
        // function refuses any character outside `[A-Za-z0-9_-]`. Even if a
        // future relaxation introduced lowercasing, the reservation table is
        // the load-bearing collision guard tested here directly: an explicit
        // second raw id targeting the same sanitized form is rejected.
        let (mgr, _dir) = manager();
        let _first = mgr.ensure(&IssueId::from("ENG-42")).unwrap();

        // Simulate a different raw id that shares the sanitized form by
        // poking the reservation table directly via a second ensure with the
        // same sanitized segment but a different raw spelling. The
        // user-facing scenario is: two distinct issue ids that map to the
        // same on-disk path must not silently share a session.
        {
            let mut table = mgr.reservations.lock().unwrap();
            // Register a fake-original raw id so the next ensure observes a
            // mismatch.
            table.insert("ENG-42".to_owned(), "original-ENG-42".to_owned());
        }
        let err = mgr.ensure(&IssueId::from("ENG-42")).unwrap_err();
        assert!(matches!(err, SessionError::Collision { .. }));
    }

    #[test]
    fn canonical_and_extended_ids_round_trip() {
        // Documented contract: ASCII alphanumerics + `_` + `-` are accepted.
        for id in ["ENG-42", "ROKI-1", "ABC-9999", "ABC_42", "A1-B2-C3"] {
            assert_eq!(sanitize_issue_id(id).unwrap(), id);
        }
    }
}
