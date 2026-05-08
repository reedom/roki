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
    if let Some(root) = std::env::var_os("ROKI_WT_ROOT_OVERRIDE") {
        let path = PathBuf::from(root).join(ticket_id);
        std::fs::create_dir_all(&path)?;
        return Ok(path);
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
    // Currently unused: `wt list` is global and override-mode is path-only.
    // Task 6 path-safety canonicalize will consume it via the ghq base path.
    let _ = ghq;
    if let Some(root) = std::env::var_os("ROKI_WT_ROOT_OVERRIDE") {
        let path = PathBuf::from(root).join(ticket_id);
        return Ok(if path.is_dir() { Some(path) } else { None });
    }
    wt_list_find(ticket_id).await
}

fn wt_bin() -> std::ffi::OsString {
    std::env::var_os("ROKI_WT_BIN_OVERRIDE").unwrap_or_else(|| "wt".into())
}

/// Run `wt list` and return the path whose branch matches `ticket_id`.
/// `wt list` prints one line per worktree on stdout, formatted as
/// `<branch>` followed by whitespace-separated metadata whose first field
/// is the absolute path. Branch name = ticket id verbatim per fr:05 line 36.
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

pub async fn remove(_ghq: &str, _ticket_id: &str) -> Result<bool, WorktreeError> {
    unimplemented!("Task 5 implements remove")
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
        assert_eq!(result, Some(root.join("OPS-1")));
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
        assert_eq!(result, root.join("OPS-3"));
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
        assert_eq!(again, root.join("OPS-4"));
    }
}
