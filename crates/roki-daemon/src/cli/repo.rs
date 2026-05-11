//! `roki repo` — resolve a ticket's worktree path (or ghq base fallback).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use thiserror::Error;

use crate::engine::{cwd, worktree};

#[derive(Debug, Args)]
pub struct RepoArgs {
    /// ghq slug (e.g., github.com/foo/bar). Defaults to $ROKI_REPO_GHQ.
    #[arg(value_name = "GHQ")]
    pub ghq: Option<String>,
    /// Ticket id. Defaults to $ROKI_TICKET_ID.
    #[arg(long = "ticket", value_name = "ID")]
    pub ticket: Option<String>,
    /// Require a materialized worktree; exit 1 otherwise.
    #[arg(long = "worktree")]
    pub worktree: bool,
    /// Run `ghq get <ghq>` before resolving the ghq base path.
    #[arg(long = "auto-clone")]
    pub auto_clone: bool,
    /// roki.toml path (optional).
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("roki repo: ghq slug required (pass <GHQ> or set $ROKI_REPO_GHQ)")]
    NoGhq,
    #[error("roki repo: ticket id required (pass --ticket or set $ROKI_TICKET_ID)")]
    NoTicket,
    #[error("roki repo: ghq get failed: {0}")]
    GhqGet(String),
    #[error("roki repo: worktree not yet materialized for ({ghq}, {ticket})")]
    NoWorktree { ghq: String, ticket: String },
    #[error("roki repo: {0}")]
    Resolve(String),
}

pub async fn run(args: RepoArgs) -> ExitCode {
    match run_inner(args).await {
        Ok(path) => {
            println!("{path}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            match err {
                RepoError::NoGhq | RepoError::NoTicket => ExitCode::from(2),
                _ => ExitCode::from(1),
            }
        }
    }
}

#[cfg(test)]
async fn run_test(args: RepoArgs) -> Result<String, RepoError> {
    run_inner(args).await
}

async fn run_inner(args: RepoArgs) -> Result<String, RepoError> {
    let ghq = args
        .ghq
        .or_else(|| {
            std::env::var("ROKI_REPO_GHQ")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .ok_or(RepoError::NoGhq)?;
    let ticket = args
        .ticket
        .or_else(|| {
            std::env::var("ROKI_TICKET_ID")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .ok_or(RepoError::NoTicket)?;

    if args.auto_clone {
        let out = tokio::process::Command::new("ghq")
            .arg("get")
            .arg(&ghq)
            .output()
            .await
            .map_err(|e| RepoError::GhqGet(format!("{e}")))?;
        if !out.status.success() {
            return Err(RepoError::GhqGet(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
    }

    if args.worktree {
        let wt = worktree::exists(&ghq, &ticket)
            .await
            .map_err(|e| RepoError::Resolve(format!("{e}")))?;
        match wt {
            Some(p) => Ok(p.to_string_lossy().into_owned()),
            None => Err(RepoError::NoWorktree { ghq, ticket }),
        }
    } else {
        let path = cwd::resolve(&ghq, &ticket)
            .await
            .map_err(|e| RepoError::Resolve(format!("{e}")))?;
        Ok(path.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn worktree_present_returns_worktree_path() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(wt_root.join("OPS-10")).unwrap();
        let ghq_base = tmp.path().join("ghq-base");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let out = temp_env::async_with_vars(
            [
                ("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap())),
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
            ],
            run_test(RepoArgs {
                ghq: Some("github.com/x/y".into()),
                ticket: Some("OPS-10".into()),
                worktree: false,
                auto_clone: false,
                config: None,
            }),
        )
        .await
        .unwrap();
        assert!(out.ends_with("OPS-10"), "got {out:?}");
    }

    #[tokio::test]
    async fn worktree_absent_returns_ghq_base() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(&wt_root).unwrap();
        let ghq_base = tmp.path().join("ghq-base");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let out = temp_env::async_with_vars(
            [
                ("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap())),
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
            ],
            run_test(RepoArgs {
                ghq: Some("github.com/x/y".into()),
                ticket: Some("OPS-10".into()),
                worktree: false,
                auto_clone: false,
                config: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::path::PathBuf::from(out), ghq_base);
    }

    #[tokio::test]
    async fn worktree_flag_strict_failure_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(&wt_root).unwrap();
        let ghq_base = tmp.path().join("ghq-base");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let err = temp_env::async_with_vars(
            [
                ("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap())),
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
            ],
            run_test(RepoArgs {
                ghq: Some("github.com/x/y".into()),
                ticket: Some("OPS-10".into()),
                worktree: true,
                auto_clone: false,
                config: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("worktree not yet materialized"));
    }
}
