//! `ghq` shellout adapter.
//!
//! Boundary: DAEMON-INTERNAL only. Phase subprocesses must never invoke
//! this module. The orchestrator + worktree manager use `GhqTool` to map
//! ghq identifiers to local clone paths.
//!
//! Spec refs: requirements.md Req 4.6, 10.1.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum GhqError {
    #[error("`ghq {subcommand}` exited with status {status}: {stderr}")]
    ExitStatus {
        subcommand: String,
        status: i32,
        stderr: String,
    },

    #[error("failed to spawn `ghq {subcommand}`: {source}")]
    Spawn {
        subcommand: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`ghq get {0}` succeeded but list still returned no path")]
    NotPresentAfterGet(String),

    #[error("`ghq` returned a path that does not exist: {0}")]
    PathMissing(PathBuf),
}

/// Daemon-internal mapping from ghq identifier (`owner/repo` or
/// `host/owner/repo`) to the local clone path.
#[async_trait]
pub trait GhqTool: Send + Sync {
    /// Return the local path for `ghq_id` if cloned, `None` otherwise.
    async fn list_path(&self, ghq_id: &str) -> Result<Option<PathBuf>, GhqError>;

    /// Ensure `ghq_id` is cloned, then return the local path.
    async fn ensure_cloned(&self, ghq_id: &str) -> Result<PathBuf, GhqError>;
}

/// Production [`GhqTool`] driver: shells out to `ghq list -p` / `ghq get`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealGhq;

impl RealGhq {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl GhqTool for RealGhq {
    async fn list_path(&self, ghq_id: &str) -> Result<Option<PathBuf>, GhqError> {
        let output = Command::new("ghq")
            .arg("list")
            .arg("-p")
            .arg(ghq_id)
            .output()
            .await
            .map_err(|err| GhqError::Spawn {
                subcommand: "list".to_owned(),
                source: err,
            })?;

        // `ghq list -p` exits non-zero when the identifier is missing; treat
        // that as "not present" rather than an error.
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let first = stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty());
        Ok(first.map(PathBuf::from))
    }

    async fn ensure_cloned(&self, ghq_id: &str) -> Result<PathBuf, GhqError> {
        if let Some(path) = self.list_path(ghq_id).await? {
            if path.exists() {
                return Ok(path);
            }
            return Err(GhqError::PathMissing(path));
        }

        let output = Command::new("ghq")
            .arg("get")
            .arg(ghq_id)
            .output()
            .await
            .map_err(|err| GhqError::Spawn {
                subcommand: "get".to_owned(),
                source: err,
            })?;

        if !output.status.success() {
            return Err(GhqError::ExitStatus {
                subcommand: "get".to_owned(),
                status: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        match self.list_path(ghq_id).await? {
            Some(path) if path.exists() => Ok(path),
            Some(path) => Err(GhqError::PathMissing(path)),
            None => Err(GhqError::NotPresentAfterGet(ghq_id.to_owned())),
        }
    }
}

/// In-memory test double for [`GhqTool`]. Maintains an explicit map of
/// `ghq_id -> on-disk path`. Tests pre-create directories under a tempdir
/// and then register them via [`MockGhq::register`].
#[derive(Debug, Default)]
pub struct MockGhq {
    inner: Mutex<MockGhqInner>,
}

#[derive(Debug, Default)]
struct MockGhqInner {
    map: HashMap<String, PathBuf>,
    list_calls: Vec<String>,
    get_calls: Vec<String>,
}

impl MockGhq {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, ghq_id: &str, path: PathBuf) {
        self.inner
            .lock()
            .unwrap()
            .map
            .insert(ghq_id.to_owned(), path);
    }

    pub fn list_calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().list_calls.clone()
    }

    pub fn get_calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().get_calls.clone()
    }
}

#[async_trait]
impl GhqTool for MockGhq {
    async fn list_path(&self, ghq_id: &str) -> Result<Option<PathBuf>, GhqError> {
        let mut inner = self.inner.lock().unwrap();
        inner.list_calls.push(ghq_id.to_owned());
        Ok(inner.map.get(ghq_id).cloned())
    }

    async fn ensure_cloned(&self, ghq_id: &str) -> Result<PathBuf, GhqError> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.get_calls.push(ghq_id.to_owned());
            if let Some(path) = inner.map.get(ghq_id).cloned() {
                if !path.exists() {
                    std::fs::create_dir_all(&path).map_err(|err| GhqError::Spawn {
                        subcommand: "MockGhq::ensure_cloned".to_owned(),
                        source: err,
                    })?;
                }
                return Ok(path);
            }
        }
        Err(GhqError::NotPresentAfterGet(ghq_id.to_owned()))
    }
}

/// Helper for tests: create a fake clone directory under `parent` and
/// register it with `mock`.
pub fn seed_mock_repo(mock: &MockGhq, parent: &Path, ghq_id: &str) -> PathBuf {
    let leaf = ghq_id.replace('/', "__");
    let path = parent.join(leaf);
    std::fs::create_dir_all(&path).expect("create seeded repo dir");
    mock.register(ghq_id, path.clone());
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_returns_some_for_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mock = MockGhq::new();
        let path = seed_mock_repo(&mock, tmp.path(), "github.com/owner/repo");
        let got = mock.list_path("github.com/owner/repo").await.unwrap();
        assert_eq!(got.as_deref(), Some(path.as_path()));
    }

    #[tokio::test]
    async fn mock_returns_none_for_missing() {
        let mock = MockGhq::new();
        assert!(mock.list_path("github.com/owner/missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mock_ensure_cloned_records_get_call() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mock = MockGhq::new();
        seed_mock_repo(&mock, tmp.path(), "github.com/owner/repo");
        let path = mock.ensure_cloned("github.com/owner/repo").await.unwrap();
        assert!(path.exists());
        assert_eq!(mock.get_calls(), vec!["github.com/owner/repo"]);
    }
}
