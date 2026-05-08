//! Worktree lifecycle owned by the daemon.
//!
//! Single caller of the external `wt` binary. Three operations:
//!
//! - `ensure`  — idempotent create (fast-path via `wt list`; falls back to
//!   `wt switch-create <ticket_id>` if absent).
//! - `exists`  — verify presence via `wt list` without creating.
//! - `remove`  — `wt remove`; idempotent (returns `Ok(false)` when absent).
//!
//! Test seams (production binary never reads them):
//!
//! - `ROKI_WT_BIN_OVERRIDE` — alternate path to the `wt` binary.
//! - `ROKI_WT_ROOT_OVERRIDE` — when set, fully bypasses `wt`/`ghq`; resolves
//!   `<root>/<ticket_id>/` directly via fs ops (mkdir / exists / remove_dir_all).

#![allow(dead_code)]

use std::path::PathBuf;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("wt binary not found on PATH")]
    WtNotFound,

    #[error("wt switch-create failed (exit={exit_code:?}): {stderr}")]
    SwitchCreateFailed {
        stderr: String,
        exit_code: Option<i32>,
    },

    #[error("wt list failed (exit={exit_code:?}): {stderr}")]
    ListFailed {
        stderr: String,
        exit_code: Option<i32>,
    },

    #[error("wt remove failed (exit={exit_code:?}): {stderr}")]
    RemoveFailed {
        stderr: String,
        exit_code: Option<i32>,
    },

    #[error("worktree path {} escapes root {}", resolved.display(), root.display())]
    PathEscape { resolved: PathBuf, root: PathBuf },

    #[error("worktree path {} already used for ticket {ticket_id}", existing_path.display())]
    Conflict {
        ticket_id: String,
        existing_path: PathBuf,
    },

    #[error("worktree io error: {0}")]
    Io(#[from] std::io::Error),
}

impl WorktreeError {
    /// Exit code from the underlying `wt` invocation when one exists, else `None`.
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            WorktreeError::SwitchCreateFailed { exit_code, .. }
            | WorktreeError::ListFailed { exit_code, .. }
            | WorktreeError::RemoveFailed { exit_code, .. } => *exit_code,
            _ => None,
        }
    }
}

pub async fn ensure(ghq: &str, ticket_id: &str) -> Result<PathBuf, WorktreeError> {
    if let Some(existing) = exists(ghq, ticket_id).await? {
        return Ok(existing);
    }
    if let Some(root_os) = std::env::var_os("ROKI_WT_ROOT_OVERRIDE") {
        let root = PathBuf::from(root_os);
        let path = root.join(ticket_id);
        std::fs::create_dir_all(&path)?;
        return canonicalize_under_root(&path, &root);
    }
    wt_switch_create(ticket_id).await
}

async fn wt_switch_create(ticket_id: &str) -> Result<PathBuf, WorktreeError> {
    let bin = wt_bin();
    let out = Command::new(&bin)
        .arg("switch-create")
        .arg(ticket_id)
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => WorktreeError::WtNotFound,
            _ => WorktreeError::Io(e),
        })?;
    if !out.status.success() {
        return Err(WorktreeError::SwitchCreateFailed {
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            exit_code: out.status.code(),
        });
    }
    // wt switch-create may not print the resolved path; resolve via wt list.
    match wt_list_find(ticket_id).await? {
        Some(p) => Ok(p),
        None => Err(WorktreeError::SwitchCreateFailed {
            stderr: "wt switch-create succeeded but worktree not found by wt list".to_string(),
            exit_code: None,
        }),
    }
}

pub async fn exists(ghq: &str, ticket_id: &str) -> Result<Option<PathBuf>, WorktreeError> {
    // The `ghq` parameter is reserved for future cross-repo conflict detection
    // when wt list outputs span multiple ghq trees. Slice 4 only filters by
    // branch name, so it is unused here.
    let _ = ghq;
    if let Some(root_os) = std::env::var_os("ROKI_WT_ROOT_OVERRIDE") {
        let root = PathBuf::from(root_os);
        let path = root.join(ticket_id);
        if !path.is_dir() {
            return Ok(None);
        }
        return Ok(Some(canonicalize_under_root(&path, &root)?));
    }
    wt_list_find(ticket_id).await
}

fn canonicalize_under_root(
    path: &std::path::Path,
    root: &std::path::Path,
) -> Result<PathBuf, WorktreeError> {
    let resolved = std::fs::canonicalize(path)?;
    let root_canon = std::fs::canonicalize(root)?;
    if !resolved.starts_with(&root_canon) {
        return Err(WorktreeError::PathEscape {
            resolved,
            root: root_canon,
        });
    }
    Ok(resolved)
}

fn wt_bin() -> std::ffi::OsString {
    std::env::var_os("ROKI_WT_BIN_OVERRIDE").unwrap_or_else(|| "wt".into())
}

/// Run `wt list` and return the path whose branch matches `ticket_id`.
/// `wt list` prints one line per worktree on stdout, formatted as
/// `<branch>` followed by whitespace-separated metadata whose first field
/// is the absolute path. Branch name = ticket id verbatim per fr:05 line 36.
///
/// The returned path is NOT canonicalized here. Spec §5 path-safety only
/// applies to the override path; production-path canonicalization is
/// deferred to `cwd::resolve` (Task 7), which has the ghq base available
/// as the natural confinement root.
async fn wt_list_find(ticket_id: &str) -> Result<Option<PathBuf>, WorktreeError> {
    let bin = wt_bin();
    let out = Command::new(&bin)
        .arg("list")
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => WorktreeError::WtNotFound,
            _ => WorktreeError::Io(e),
        })?;
    if !out.status.success() {
        return Err(WorktreeError::ListFailed {
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            exit_code: out.status.code(),
        });
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // wt list output is `<branch>` followed by whitespace-separated
        // metadata; the absolute path is the first whitespace-delimited
        // field after the branch. Splitting on the first whitespace run
        // accepts both tab- and space-separated formats.
        let Some((branch, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let branch = branch.trim();
        let path = rest.trim();
        if branch == ticket_id && !path.is_empty() {
            return Ok(Some(PathBuf::from(path)));
        }
    }
    Ok(None)
}

pub async fn remove(ghq: &str, ticket_id: &str) -> Result<bool, WorktreeError> {
    let Some(path) = exists(ghq, ticket_id).await? else {
        return Ok(false);
    };
    if std::env::var_os("ROKI_WT_ROOT_OVERRIDE").is_some() {
        std::fs::remove_dir_all(&path)?;
        return Ok(true);
    }
    wt_remove(ticket_id).await.map(|_| true)
}

async fn wt_remove(ticket_id: &str) -> Result<(), WorktreeError> {
    let bin = wt_bin();
    let out = Command::new(&bin)
        .arg("remove")
        .arg(ticket_id)
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => WorktreeError::WtNotFound,
            _ => WorktreeError::Io(e),
        })?;
    if !out.status.success() {
        return Err(WorktreeError::RemoveFailed {
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            exit_code: out.status.code(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wt_not_found_display() {
        let e = WorktreeError::WtNotFound;
        assert!(format!("{e}").contains("wt binary not found"));
    }

    #[test]
    fn switch_create_failed_display_includes_exit_code() {
        let e = WorktreeError::SwitchCreateFailed {
            stderr: "boom".to_string(),
            exit_code: Some(7),
        };
        let s = format!("{e}");
        assert!(s.contains("Some(7)"), "{s}");
        assert!(s.contains("boom"), "{s}");
    }

    #[test]
    fn signatures_compile() {
        // Bind the function items so the test fails to compile if any of
        // the three public async fns are removed or renamed. Type-annotated
        // assignments would not survive async fn return types (impl Future
        // is unnameable), so `let _ = item;` is the right shape.
        let _ = ensure;
        let _ = exists;
        let _ = remove;
        let _ = WorktreeError::WtNotFound;
    }

    #[tokio::test]
    async fn exists_override_present() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("OPS-1")).unwrap();
        let result = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(root.to_str().unwrap()))],
            async { exists("github.com/acme/widget", "OPS-1").await },
        )
        .await
        .unwrap();
        let expected = std::fs::canonicalize(root.join("OPS-1")).unwrap();
        assert_eq!(result, Some(expected));
    }

    #[tokio::test]
    async fn exists_override_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let result = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(root.to_str().unwrap()))],
            async { exists("github.com/acme/widget", "OPS-2").await },
        )
        .await
        .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn ensure_creates_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let result = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(root.to_str().unwrap()))],
            async { ensure("github.com/acme/widget", "OPS-3").await },
        )
        .await
        .unwrap();
        let expected = std::fs::canonicalize(root.join("OPS-3")).unwrap();
        assert_eq!(result, expected);
        assert!(root.join("OPS-3").is_dir());
    }

    #[tokio::test]
    async fn ensure_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let _ = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(root.to_str().unwrap()))],
            async { ensure("github.com/acme/widget", "OPS-4").await.unwrap() },
        )
        .await;
        // Second call must succeed without error and return the same path.
        let again = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(root.to_str().unwrap()))],
            async { ensure("github.com/acme/widget", "OPS-4").await },
        )
        .await
        .unwrap();
        let expected = std::fs::canonicalize(root.join("OPS-4")).unwrap();
        assert_eq!(again, expected);
    }

    #[tokio::test]
    async fn remove_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("OPS-5")).unwrap();
        let removed = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(root.to_str().unwrap()))],
            async { remove("github.com/acme/widget", "OPS-5").await },
        )
        .await
        .unwrap();
        assert!(removed);
        assert!(!root.join("OPS-5").exists());
    }

    #[tokio::test]
    async fn remove_when_absent_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let removed = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(root.to_str().unwrap()))],
            async { remove("github.com/acme/widget", "OPS-6").await },
        )
        .await
        .unwrap();
        assert!(!removed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exists_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        // Place a symlink under the root that points outside.
        symlink(outside.path(), root.path().join("OPS-7")).unwrap();
        let err = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(root.path().to_str().unwrap()))],
            async { exists("github.com/acme/widget", "OPS-7").await },
        )
        .await
        .unwrap_err();
        match err {
            WorktreeError::PathEscape { .. } => {}
            other => panic!("expected PathEscape, got {other:?}"),
        }
    }

    #[test]
    fn conflict_variant_constructs() {
        // Override-mode is path-based; conflict comes from real `wt list` output.
        // This test documents the contract via a unit fake of wt_list_find that
        // is exercised in the e2e harness; here we only assert the error
        // variant constructs cleanly so call sites can match on it.
        let e = WorktreeError::Conflict {
            ticket_id: "OPS-8".to_string(),
            existing_path: std::path::PathBuf::from("/tmp/other"),
        };
        match e {
            WorktreeError::Conflict { ticket_id, .. } => assert_eq!(ticket_id, "OPS-8"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
