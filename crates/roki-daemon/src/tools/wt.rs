//! `wt` (worktrunk) external CLI wrapper used by the workspace boundary.
//!
//! Ported from monorail's `src/tools/wt.rs` per task 6.1
//! (`.kiro/specs/roki-mvp/design-worktree-workspace.md`). The trait is the
//! seam the workspace manager depends on; [`RealWt`] shells out to the
//! installed `wt` binary. Tests inject hand-rolled mocks via the trait so
//! the CI host does not need `wt` on `$PATH`.
//!
//! ## Path layout
//!
//! `wt switch --create <branch>` creates a worktree at
//! `{repo_path}/../{repo_name}.{branch_sanitized}` (a sibling of the source
//! repo). [`RealWt::switch_create`] returns that path verbatim so the
//! workspace manager can hand it to the engine adapter as the worker CWD.
//!
//! ## Branch sanitization
//!
//! Characters outside `[A-Za-z0-9_-]` are replaced with `-`. This matches the
//! reference monorail implementation and is the only sanitizer applied to
//! Linear issue identifiers when they are used as branch names.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;

/// External-tool error surface returned by [`WtTool`] implementations. Kept
/// narrow on purpose: the workspace manager translates these into its own
/// `WorkspaceError` for the orchestrator.
#[derive(Debug, Error)]
pub enum WtError {
    /// The `wt` binary was not found on `PATH`. Surfaced as `Spawn` from
    /// `tokio::process` when the OS reports `ErrorKind::NotFound`. The
    /// bootstrap is expected to detect this earlier (see `runtime::run_with_shutdown`).
    #[error("wt binary not found on PATH: {message}")]
    NotFound { message: String },

    /// `wt` was invoked but exited non-zero. The captured stderr is in
    /// `message` so the orchestrator can include it in its escalation log.
    #[error("wt exited non-zero: {message}")]
    NonZeroExit { message: String },

    /// IO failure while spawning or waiting on the child process.
    #[error("wt spawn/io error: {message}")]
    Io { message: String },

    /// `repo_path` had no parent or no file_name component. Indicates a
    /// programmer error upstream (the `ghq` lookup returned a degenerate
    /// path) rather than a `wt` failure.
    #[error("invalid repo path for wt: {message}")]
    InvalidRepoPath { message: String },
}

/// Outbound port the workspace manager depends on. Implementations must be
/// `Send + Sync` so the manager can be shared across worker tasks.
#[async_trait]
pub trait WtTool: Send + Sync {
    /// Create (or switch to) a worktree at
    /// `{repo_path}/../{repo_name}.{branch_sanitized}`. Returns the worktree
    /// path. The branch is sanitized via [`sanitize_branch`].
    async fn switch_create(&self, repo_path: &Path, branch: &str) -> Result<PathBuf, WtError>;

    /// Remove the worktree at `worktree_path`. Returns `Ok(())` on success.
    /// Does NOT delete the underlying branch — `wt remove` is documented to
    /// preserve branches per the locked design decisions.
    async fn remove(&self, worktree_path: &Path) -> Result<(), WtError>;
}

/// Sanitize a Linear issue id for safe use as a Git branch / worktree path
/// suffix. Characters outside `[A-Za-z0-9_-]` collapse to `-`. Empty inputs
/// pass through unchanged (the caller rejects them earlier).
pub fn sanitize_branch(branch: &str) -> String {
    branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Compute the worktree path `wt switch --create` will produce for
/// `(repo_path, branch)`, without invoking `wt`. The caller uses this for
/// the deterministic `remove` path and for collision detection.
pub fn worktree_path_for(repo_path: &Path, branch: &str) -> Result<PathBuf, WtError> {
    let parent = repo_path.parent().ok_or_else(|| WtError::InvalidRepoPath {
        message: format!("repo path `{}` has no parent", repo_path.display()),
    })?;
    let repo_name = repo_path
        .file_name()
        .ok_or_else(|| WtError::InvalidRepoPath {
            message: format!("repo path `{}` has no file name", repo_path.display()),
        })?
        .to_string_lossy()
        .into_owned();
    let sanitized = sanitize_branch(branch);
    Ok(parent.join(format!("{repo_name}.{sanitized}")))
}

/// Production [`WtTool`] backed by the `wt` binary on `$PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealWt;

impl RealWt {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl WtTool for RealWt {
    async fn switch_create(&self, repo_path: &Path, branch: &str) -> Result<PathBuf, WtError> {
        let output = Command::new("wt")
            .arg("-C")
            .arg(repo_path)
            .args(["switch", "--create", branch])
            .output()
            .await
            .map_err(|err| classify_io(err, "switch_create"))?;
        if !output.status.success() {
            return Err(WtError::NonZeroExit {
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        worktree_path_for(repo_path, branch)
    }

    async fn remove(&self, worktree_path: &Path) -> Result<(), WtError> {
        let output = Command::new("wt")
            .arg("-C")
            .arg(worktree_path)
            .args(["remove"])
            .output()
            .await
            .map_err(|err| classify_io(err, "remove"))?;
        if !output.status.success() {
            return Err(WtError::NonZeroExit {
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(())
    }
}

fn classify_io(err: std::io::Error, op: &'static str) -> WtError {
    if err.kind() == std::io::ErrorKind::NotFound {
        WtError::NotFound {
            message: format!("`wt` not found while running {op}"),
        }
    } else {
        WtError::Io {
            message: format!("{op}: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_slashes_with_dashes() {
        // Mirrors monorail's verbatim test case: an issue id containing '/'
        // collapses to '-'.
        assert_eq!(sanitize_branch("ACM-1/x"), "ACM-1-x");
    }

    #[test]
    fn sanitize_passes_safe_characters_unchanged() {
        assert_eq!(sanitize_branch("ENG-42_v2"), "ENG-42_v2");
    }

    #[test]
    fn sanitize_replaces_spaces_and_punctuation() {
        assert_eq!(sanitize_branch("ENG 42!bug"), "ENG-42-bug");
    }

    #[test]
    fn sanitize_replaces_unicode_with_dashes() {
        // Branch names must stay ASCII for filesystem-safety; non-ASCII
        // characters collapse to '-' per the locked decision.
        assert_eq!(sanitize_branch("issue-é-42"), "issue---42");
    }

    #[test]
    fn worktree_path_uses_sibling_layout() {
        // Locked decision #4: worktree path = `{repo_path}/../{repo_name}.{branch_sanitized}`.
        let repo = Path::new("/tmp/parent/myrepo");
        let path = worktree_path_for(repo, "ENG-1").expect("path");
        assert_eq!(path, Path::new("/tmp/parent/myrepo.ENG-1"));
    }

    #[test]
    fn worktree_path_sanitizes_branch_in_suffix() {
        let repo = Path::new("/tmp/parent/myrepo");
        let path = worktree_path_for(repo, "ENG/42").expect("path");
        assert_eq!(path, Path::new("/tmp/parent/myrepo.ENG-42"));
    }

    #[test]
    fn worktree_path_rejects_root_path() {
        // `/` has no parent. The wrapper must surface a typed error rather
        // than panic via a missing `parent`.
        let path = worktree_path_for(Path::new("/"), "ENG-1");
        assert!(matches!(path, Err(WtError::InvalidRepoPath { .. })));
    }

    #[tokio::test]
    async fn real_wt_returns_not_found_when_binary_absent() {
        // We cannot guarantee `wt` is missing on every CI host; we therefore
        // tolerate either NotFound (binary truly absent) or NonZeroExit (binary
        // present but the synthetic repo path is not a Git repo). The test
        // exists to assert the error classification surface compiles and
        // reports a typed error rather than panicking.
        let wt = RealWt::new();
        let res = wt
            .switch_create(Path::new("/nonexistent/roki-test-repo"), "ENG-1")
            .await;
        match res {
            Err(WtError::NotFound { .. }) | Err(WtError::NonZeroExit { .. }) => {}
            other => panic!(
                "expected NotFound or NonZeroExit from RealWt against bogus path; got {other:?}"
            ),
        }
    }
}
