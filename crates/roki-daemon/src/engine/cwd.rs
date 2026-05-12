//! Single cwd decision site for phase subprocesses.
//!
//! Returns the worktree path when one exists for `(ghq, ticket_id)`, else the
//! ghq base path. Per fr:04 line 46 / fr:05 line 34:
//!
//! - Session-shape supervisors call this once at cycle start; the result is
//!   pinned for the entire cycle.
//! - Command-shape phase invocations call this per spawn so the cwd reflects
//!   current worktree state (worktree may have been created mid-cycle).

use std::path::PathBuf;

use tokio::process::Command;

use crate::engine::worktree;
use crate::error::PhaseInfraError;

pub async fn resolve(ghq: &str, ticket_id: &str) -> Result<PathBuf, PhaseInfraError> {
    match worktree::exists(ghq, ticket_id).await {
        Ok(Some(path)) => Ok(path),
        Ok(None) => resolve_ghq_base(ghq).await,
        Err(err) => {
            let exit_code = err.exit_code();
            Err(PhaseInfraError::WorktreeError {
                error_text: format!("{err}"),
                exit_code,
            })
        }
    }
}

/// Resolve the absolute path of the operator's checkout via
/// `ghq list -p <ghq>`. Returns `RepoNotFound` when ghq has no entry.
///
/// Test-support seam: when `ROKI_GHQ_BASE_OVERRIDE` is set to a non-empty
/// value, the override path is returned verbatim; production env never has
/// it set.
pub async fn resolve_ghq_base(ghq: &str) -> Result<PathBuf, PhaseInfraError> {
    if let Ok(override_path) = std::env::var("ROKI_GHQ_BASE_OVERRIDE") {
        if !override_path.is_empty() {
            return Ok(PathBuf::from(override_path));
        }
    }
    let out = Command::new("ghq")
        .arg("list")
        .arg("-p")
        .arg(ghq)
        .output()
        .await
        .map_err(|source| PhaseInfraError::Spawn {
            cmd: format!("ghq list -p {ghq}"),
            source,
        })?;
    if !out.status.success() {
        return Err(PhaseInfraError::RepoNotFound {
            ghq: ghq.to_string(),
        });
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PhaseInfraError::RepoNotFound {
            ghq: ghq.to_string(),
        })?;
    Ok(PathBuf::from(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn falls_back_to_ghq_when_no_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(&wt_root).unwrap();
        let ghq_base = tmp.path().join("ghq-base");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let result = temp_env::async_with_vars(
            [
                ("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap())),
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
            ],
            async { resolve("github.com/acme/widget", "OPS-9").await },
        )
        .await
        .unwrap();
        assert_eq!(result, ghq_base);
    }

    #[tokio::test]
    async fn returns_worktree_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(wt_root.join("OPS-10")).unwrap();
        let ghq_base = tmp.path().join("ghq-base");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let result = temp_env::async_with_vars(
            [
                ("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap())),
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
            ],
            async { resolve("github.com/acme/widget", "OPS-10").await },
        )
        .await
        .unwrap();
        // canonicalize_under_root resolves symlinks; compare via canonicalize.
        let expected = std::fs::canonicalize(wt_root.join("OPS-10")).unwrap();
        assert_eq!(result, expected);
    }
}
