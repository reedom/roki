//! `wt`/`git worktree` shellout adapter.
//!
//! Boundary: DAEMON-INTERNAL only. Phase subprocesses must never invoke
//! this module — they reach git via their own Bash tool. The daemon owns
//! worktree creation and removal so the lifecycle is reconcilable on
//! restart.
//!
//! Spec refs: requirements.md Req 4.6.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;

/// Errors returned by [`WtTool`] implementations.
#[derive(Debug, Error)]
pub enum WtError {
    #[error("repo path `{0}` is not absolute or does not exist")]
    InvalidRepoPath(PathBuf),

    #[error("repo path `{0}` has no parent (cannot place sibling worktree)")]
    NoParent(PathBuf),

    #[error("repo path `{0}` has no file name")]
    NoRepoName(PathBuf),

    #[error("branch name `{0}` is empty after sanitization")]
    EmptyBranch(String),

    #[error("`{tool}` exited with status {status}: {stderr}")]
    ExitStatus {
        tool: String,
        status: i32,
        stderr: String,
    },

    #[error("failed to spawn `{tool}`: {source}")]
    Spawn {
        tool: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse `wt list --porcelain` output: {0}")]
    ParseError(String),
}

/// One entry returned from `wt list --porcelain` (or the git fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Daemon-internal worktree manipulation surface.
#[async_trait]
pub trait WtTool: Send + Sync {
    /// Create-or-switch a worktree for `branch` rooted at `repo_path`. The
    /// returned path is the absolute on-disk worktree directory.
    async fn switch_create(
        &self,
        repo_path: &Path,
        branch: &str,
    ) -> Result<PathBuf, WtError>;

    /// List the worktrees registered for the repository at `repo_path`.
    async fn list_porcelain(
        &self,
        repo_path: &Path,
    ) -> Result<Vec<WorktreeEntry>, WtError>;

    /// Remove the worktree at `worktree_path`. The branch underneath is
    /// preserved.
    async fn remove(&self, worktree_path: &Path) -> Result<(), WtError>;
}

/// Compute the deterministic sibling path used by both `RealWt` and the
/// recovery scanner: `{repo_parent}/{repo_name}.{sanitized_branch}`.
///
/// Branch characters outside `[A-Za-z0-9_-]` collapse to `-` so a `feature/x`
/// branch becomes `repo.feature-x` on disk. The sanitization is one-way and
/// loss-tolerant; the daemon retains the original branch separately.
pub fn worktree_path_for(repo_path: &Path, branch: &str) -> Result<PathBuf, WtError> {
    let parent = repo_path
        .parent()
        .ok_or_else(|| WtError::NoParent(repo_path.to_path_buf()))?;
    let repo_name = repo_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| WtError::NoRepoName(repo_path.to_path_buf()))?;

    let sanitized: String = branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    if sanitized.trim_matches('-').is_empty() {
        return Err(WtError::EmptyBranch(branch.to_owned()));
    }

    Ok(parent.join(format!("{repo_name}.{sanitized}")))
}

/// Production [`WtTool`] driver: shells out to the operator-installed `wt`
/// binary (with a `git worktree` fallback for the porcelain listing).
#[derive(Debug, Default, Clone, Copy)]
pub struct RealWt;

impl RealWt {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl WtTool for RealWt {
    async fn switch_create(
        &self,
        repo_path: &Path,
        branch: &str,
    ) -> Result<PathBuf, WtError> {
        let output = Command::new("wt")
            .arg("switch-create")
            .arg(branch)
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|err| WtError::Spawn {
                tool: "wt switch-create".to_owned(),
                source: err,
            })?;

        if !output.status.success() {
            return Err(WtError::ExitStatus {
                tool: "wt switch-create".to_owned(),
                status: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        // wt prints the worktree path; if absent, fall back to the
        // deterministic computation so we always return something usable.
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !stdout.is_empty() {
            let candidate = PathBuf::from(stdout);
            if candidate.is_absolute() {
                return Ok(candidate);
            }
        }
        worktree_path_for(repo_path, branch)
    }

    async fn list_porcelain(
        &self,
        repo_path: &Path,
    ) -> Result<Vec<WorktreeEntry>, WtError> {
        // Prefer `wt list --porcelain`; fall back to `git worktree list
        // --porcelain` so we tolerate `wt` versions that lack the flag.
        let primary = Command::new("wt")
            .arg("list")
            .arg("--porcelain")
            .current_dir(repo_path)
            .output()
            .await;

        let output = match primary {
            Ok(out) if out.status.success() => out,
            _ => Command::new("git")
                .arg("worktree")
                .arg("list")
                .arg("--porcelain")
                .current_dir(repo_path)
                .output()
                .await
                .map_err(|err| WtError::Spawn {
                    tool: "git worktree list".to_owned(),
                    source: err,
                })?,
        };

        if !output.status.success() {
            return Err(WtError::ExitStatus {
                tool: "git worktree list".to_owned(),
                status: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        parse_porcelain(&String::from_utf8_lossy(&output.stdout))
    }

    async fn remove(&self, worktree_path: &Path) -> Result<(), WtError> {
        let output = Command::new("wt")
            .arg("remove")
            .arg(worktree_path)
            .output()
            .await
            .map_err(|err| WtError::Spawn {
                tool: "wt remove".to_owned(),
                source: err,
            })?;

        if !output.status.success() {
            return Err(WtError::ExitStatus {
                tool: "wt remove".to_owned(),
                status: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(())
    }
}

/// Parse the `git worktree list --porcelain` (and matching `wt list
/// --porcelain`) output into [`WorktreeEntry`] records. Records are
/// separated by blank lines; recognized prefixes are `worktree <path>` and
/// `branch refs/heads/<name>` (or `detached`).
fn parse_porcelain(text: &str) -> Result<Vec<WorktreeEntry>, WtError> {
    let mut out = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    let flush =
        |out: &mut Vec<WorktreeEntry>, p: &mut Option<PathBuf>, b: &mut Option<String>| {
            if let Some(path) = p.take() {
                out.push(WorktreeEntry {
                    path,
                    branch: b.take(),
                });
            } else {
                // No path collected yet; reset branch buffer too.
                *b = None;
            }
        };

    for line in text.lines() {
        if line.is_empty() {
            flush(&mut out, &mut current_path, &mut current_branch);
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            // A new "worktree" line starts a fresh record.
            flush(&mut out, &mut current_path, &mut current_branch);
            current_path = Some(PathBuf::from(path.trim()));
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current_branch = Some(
                branch
                    .trim()
                    .trim_start_matches("refs/heads/")
                    .to_owned(),
            );
        } else if line == "detached" {
            current_branch = None;
        }
        // Other porcelain lines (`HEAD <sha>`, `bare`, `locked`) are
        // ignored — we only need (path, branch) pairs.
    }
    flush(&mut out, &mut current_path, &mut current_branch);
    Ok(out)
}

/// In-memory test double for [`WtTool`]. Records every `switch_create` and
/// `remove` call and materializes worktree directories on disk so callers
/// that probe existence (e.g., the recovery scanner) see real paths.
#[derive(Debug, Default)]
pub struct MockWt {
    inner: Mutex<MockWtInner>,
}

#[derive(Debug, Default)]
struct MockWtInner {
    switch_create_calls: Vec<(PathBuf, String)>,
    remove_calls: Vec<PathBuf>,
    /// Pre-seeded entries returned from `list_porcelain`, keyed by repo path.
    seeded_lists: std::collections::HashMap<PathBuf, Vec<WorktreeEntry>>,
}

impl MockWt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn switch_create_calls(&self) -> Vec<(PathBuf, String)> {
        self.inner.lock().unwrap().switch_create_calls.clone()
    }

    pub fn remove_calls(&self) -> Vec<PathBuf> {
        self.inner.lock().unwrap().remove_calls.clone()
    }

    /// Pre-seed the `list_porcelain` reply for a given repo path. Subsequent
    /// `switch_create` calls additionally append an entry to the list so a
    /// follow-up ensure short-circuits.
    pub fn seed_list(&self, repo_path: &Path, entries: Vec<WorktreeEntry>) {
        self.inner
            .lock()
            .unwrap()
            .seeded_lists
            .insert(repo_path.to_path_buf(), entries);
    }
}

#[async_trait]
impl WtTool for MockWt {
    async fn switch_create(
        &self,
        repo_path: &Path,
        branch: &str,
    ) -> Result<PathBuf, WtError> {
        let path = worktree_path_for(repo_path, branch)?;
        std::fs::create_dir_all(&path).map_err(|err| WtError::Spawn {
            tool: "MockWt::switch_create".to_owned(),
            source: err,
        })?;
        let mut inner = self.inner.lock().unwrap();
        inner
            .switch_create_calls
            .push((repo_path.to_path_buf(), branch.to_owned()));
        let entry = WorktreeEntry {
            path: path.clone(),
            branch: Some(branch.to_owned()),
        };
        inner
            .seeded_lists
            .entry(repo_path.to_path_buf())
            .or_default()
            .push(entry);
        Ok(path)
    }

    async fn list_porcelain(
        &self,
        repo_path: &Path,
    ) -> Result<Vec<WorktreeEntry>, WtError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .seeded_lists
            .get(repo_path)
            .cloned()
            .unwrap_or_default())
    }

    async fn remove(&self, worktree_path: &Path) -> Result<(), WtError> {
        if worktree_path.exists() {
            std::fs::remove_dir_all(worktree_path).map_err(|err| WtError::Spawn {
                tool: "MockWt::remove".to_owned(),
                source: err,
            })?;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.remove_calls.push(worktree_path.to_path_buf());
        for entries in inner.seeded_lists.values_mut() {
            entries.retain(|entry| entry.path != worktree_path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_path_sanitizes_branch() {
        let repo = PathBuf::from("/tmp/owner/repo");
        let path = worktree_path_for(&repo, "feature/x_y").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/owner/repo.feature-x_y"));
    }

    #[test]
    fn worktree_path_preserves_safe_branch() {
        let repo = PathBuf::from("/tmp/owner/repo");
        let path = worktree_path_for(&repo, "ENG-42").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/owner/repo.ENG-42"));
    }

    #[test]
    fn empty_branch_after_sanitization_is_rejected() {
        let repo = PathBuf::from("/tmp/owner/repo");
        let err = worktree_path_for(&repo, "////").unwrap_err();
        assert!(matches!(err, WtError::EmptyBranch(_)));
    }

    #[test]
    fn parse_porcelain_extracts_path_and_branch() {
        let text = "\
worktree /home/user/repo
HEAD deadbeef
branch refs/heads/main

worktree /home/user/repo.ENG-42
HEAD cafebabe
branch refs/heads/ENG-42

worktree /home/user/repo.detached
HEAD 1234abcd
detached
";
        let entries = parse_porcelain(text).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, PathBuf::from("/home/user/repo"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].path, PathBuf::from("/home/user/repo.ENG-42"));
        assert_eq!(entries[1].branch.as_deref(), Some("ENG-42"));
        assert_eq!(entries[2].path, PathBuf::from("/home/user/repo.detached"));
        assert!(entries[2].branch.is_none());
    }

    #[tokio::test]
    async fn mock_wt_records_calls_and_materializes_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let mock = MockWt::new();
        let path = mock.switch_create(&repo, "ENG-42").await.unwrap();
        assert!(path.exists());
        assert_eq!(mock.switch_create_calls().len(), 1);
        let listed = mock.list_porcelain(&repo).await.unwrap();
        assert!(listed.iter().any(|e| e.branch.as_deref() == Some("ENG-42")));

        mock.remove(&path).await.unwrap();
        assert!(!path.exists());
        assert_eq!(mock.remove_calls(), vec![path]);
    }
}
