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

pub async fn ensure(_ghq: &str, _ticket_id: &str) -> Result<PathBuf, WorktreeError> {
    unimplemented!("Task 4 implements ensure")
}

pub async fn exists(_ghq: &str, _ticket_id: &str) -> Result<Option<PathBuf>, WorktreeError> {
    unimplemented!("Task 3 implements exists")
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
}
