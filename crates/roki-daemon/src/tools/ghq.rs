//! `ghq` external CLI wrapper used by the workspace boundary.
//!
//! Ported from monorail's `src/tools/ghq.rs` per task 6.1
//! (`.kiro/specs/roki-mvp/design-worktree-workspace.md`). The trait is the
//! seam the workspace manager depends on; [`RealGhq`] shells out to the
//! installed `ghq` binary. Tests inject hand-rolled mocks via the trait so
//! the CI host does not need `ghq` on `$PATH`.
//!
//! ## Semantics
//!
//! * [`GhqTool::list_path`] returns `Ok(None)` when the identifier is unknown
//!   to `ghq`, and `Err` when the underlying invocation could not be
//!   classified (IO error other than `NotFound`).
//! * [`GhqTool::ensure_cloned`] performs lookup-or-clone: it tries
//!   `list_path` first, then `ghq get` on miss, then verifies the clone with
//!   a second `list_path`.
//!
//! These match monorail's behaviour verbatim.

use std::path::PathBuf;

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;

/// External-tool error surface returned by [`GhqTool`] implementations.
#[derive(Debug, Error)]
pub enum GhqError {
    /// The `ghq` binary was not found on `PATH`.
    #[error("ghq binary not found on PATH: {message}")]
    NotFound { message: String },

    /// `ghq` exited non-zero. The captured stderr is in `message`.
    #[error("ghq exited non-zero: {message}")]
    NonZeroExit { message: String },

    /// IO failure while spawning or waiting on the child process.
    #[error("ghq spawn/io error: {message}")]
    Io { message: String },

    /// `ghq get` succeeded but the subsequent `list_path` lookup still
    /// returned `None`. Indicates a `ghq` configuration drift the daemon
    /// cannot recover from automatically.
    #[error("ghq get succeeded but `{identifier}` still not found via list -p")]
    NotFoundAfterGet { identifier: String },
}

/// Outbound port the workspace manager depends on for repo path discovery.
#[async_trait]
pub trait GhqTool: Send + Sync {
    /// Look up the local checkout path for `full` (e.g., `owner/repo` or
    /// `host/owner/repo`). Returns `Ok(None)` when `ghq` is present but the
    /// identifier is unknown.
    async fn list_path(&self, full: &str) -> Result<Option<PathBuf>, GhqError>;

    /// Lookup-or-clone: returns the checkout path, cloning via `ghq get`
    /// when the identifier is unknown.
    async fn ensure_cloned(&self, full: &str) -> Result<PathBuf, GhqError>;
}

/// Production [`GhqTool`] backed by the `ghq` binary on `$PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealGhq;

impl RealGhq {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl GhqTool for RealGhq {
    async fn list_path(&self, full: &str) -> Result<Option<PathBuf>, GhqError> {
        let output = Command::new("ghq")
            .args(["list", "-p", full])
            .output()
            .await
            .map_err(|err| classify_io(err, "list -p"))?;
        if !output.status.success() {
            // ghq returns non-zero when the identifier is unknown; that is
            // the documented "missing" signal, not an error.
            return Ok(None);
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(PathBuf::from(trimmed)))
    }

    async fn ensure_cloned(&self, full: &str) -> Result<PathBuf, GhqError> {
        if let Some(path) = self.list_path(full).await? {
            return Ok(path);
        }
        let output = Command::new("ghq")
            .args(["get", full])
            .output()
            .await
            .map_err(|err| classify_io(err, "get"))?;
        if !output.status.success() {
            return Err(GhqError::NonZeroExit {
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        match self.list_path(full).await? {
            Some(path) => Ok(path),
            None => Err(GhqError::NotFoundAfterGet {
                identifier: full.to_string(),
            }),
        }
    }
}

fn classify_io(err: std::io::Error, op: &'static str) -> GhqError {
    if err.kind() == std::io::ErrorKind::NotFound {
        GhqError::NotFound {
            message: format!("`ghq` not found while running {op}"),
        }
    } else {
        GhqError::Io {
            message: format!("{op}: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_path_against_unknown_identifier_returns_none_or_typed_error() {
        // `ghq list -p` against an obviously-bogus identifier returns a
        // non-zero status when ghq is installed (so we observe Ok(None)) and
        // a `NotFound` error when ghq is absent. Either is acceptable; what
        // matters is that the wrapper does not panic and never returns
        // `Some(non_existent_path)`.
        let ghq = RealGhq::new();
        match ghq.list_path("nonexistent/roki-test-repo-zzz-12345").await {
            Ok(None) => {}
            Err(GhqError::NotFound { .. }) => {}
            other => panic!(
                "expected Ok(None) or NotFound from RealGhq against bogus identifier; got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn ensure_cloned_distinguishes_missing_binary_from_clone_failure() {
        // Same tolerance as above: when ghq is absent we expect NotFound;
        // when it's installed we expect NonZeroExit (it cannot resolve the
        // bogus identifier) or NotFoundAfterGet (it tried, succeeded, but
        // still couldn't list the path — implausible but typed).
        let ghq = RealGhq::new();
        match ghq
            .ensure_cloned("nonexistent/roki-test-repo-zzz-67890")
            .await
        {
            Err(GhqError::NotFound { .. })
            | Err(GhqError::NonZeroExit { .. })
            | Err(GhqError::NotFoundAfterGet { .. })
            | Err(GhqError::Io { .. }) => {}
            other => panic!(
                "expected typed error from RealGhq::ensure_cloned against bogus identifier; got {other:?}"
            ),
        }
    }
}
