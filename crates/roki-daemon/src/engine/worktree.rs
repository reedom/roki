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

#[derive(Debug)]
pub enum WorktreeError {
    WtNotFound,
    SwitchCreateFailed {
        stderr: String,
        exit_code: Option<i32>,
    },
    ListFailed {
        stderr: String,
        exit_code: Option<i32>,
    },
    RemoveFailed {
        stderr: String,
        exit_code: Option<i32>,
    },
    PathEscape {
        resolved: PathBuf,
        root: PathBuf,
    },
    Conflict {
        ticket_id: String,
        existing_path: PathBuf,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorktreeError::WtNotFound => write!(f, "wt binary not found on PATH"),
            WorktreeError::SwitchCreateFailed { stderr, exit_code } => {
                write!(f, "wt switch-create failed (exit={exit_code:?}): {stderr}")
            }
            WorktreeError::ListFailed { stderr, exit_code } => {
                write!(f, "wt list failed (exit={exit_code:?}): {stderr}")
            }
            WorktreeError::RemoveFailed { stderr, exit_code } => {
                write!(f, "wt remove failed (exit={exit_code:?}): {stderr}")
            }
            WorktreeError::PathEscape { resolved, root } => {
                write!(f, "worktree path {resolved:?} escapes root {root:?}")
            }
            WorktreeError::Conflict {
                ticket_id,
                existing_path,
            } => {
                write!(
                    f,
                    "worktree path {existing_path:?} already used for ticket {ticket_id}"
                )
            }
            WorktreeError::Io(e) => write!(f, "worktree io error: {e}"),
        }
    }
}

impl std::error::Error for WorktreeError {}

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

    #[tokio::test]
    async fn error_display_round_trip() {
        let e = WorktreeError::WtNotFound;
        assert!(format!("{e}").contains("wt binary not found"));
    }

    #[tokio::test]
    async fn signatures_compile() {
        // Pure type-level check: the symbols exist.
        let _ = WorktreeError::WtNotFound;
    }
}
