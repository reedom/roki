//! `roki_open_worktree` agent tool (task 7.1d).
//!
//! Daemon-owned agent surface that lets the worker open a git worktree in
//! one of the configured repos. The tool is idempotent — calling twice with
//! the same repo for the same issue returns the same path without re-running
//! `ghq` / `wt`.
//!
//! ## Allowlist enforcement
//!
//! Per design decision #3, the tool refuses any `repo` not declared in the
//! daemon's `[[repos]]` allowlist. Allowlist enforcement happens before any
//! external invocation so a hijacked agent cannot induce arbitrary `ghq get`
//! work.
//!
//! ## Issue scoping
//!
//! Each tool instance is bound to one [`IssueId`] at construction time so
//! the agent surface is naturally scoped to the worker that owns it. The
//! orchestrator constructs one tool per worker; tool dispatch through the
//! shared registry only ever reaches the tool whose issue id matches the
//! caller.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::orchestrator::state::{IssueId, RepoId};
use crate::tools::{GhqTool, Tool, ToolError, WtTool};
use crate::worktrees::{BranchName, WorktreeRegistry};

/// Stable kebab-style tool name. Mirrors the existing `linear_graphql`
/// naming convention.
pub const TOOL_NAME: &str = "roki_open_worktree";

/// Verbatim agent-facing description. The reviewer greps for this exact
/// string — do not edit without updating the spec.
pub const TOOL_DESCRIPTION: &str = "Open a git worktree for the current Linear issue in one of the configured repos. The daemon resolves the repo via ghq, creates a worktree branch named after the issue id via wt, and returns the absolute path. Idempotent — calling twice with the same repo returns the same path. Use this once per repo you intend to modify; cross-repo tickets call this multiple times.";

/// JSON-Schema document for the tool's input shape.
pub const TOOL_INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "repo": {
      "type": "string",
      "description": "Ghq identifier (owner/name or host/owner/name) of a configured repo. Must be in the daemon's allowlist."
    }
  },
  "required": ["repo"],
  "additionalProperties": false
}"#;

/// JSON-Schema document for the tool's output shape.
pub const TOOL_OUTPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "path": { "type": "string" },
    "repo": { "type": "string" },
    "branch": { "type": "string" }
  },
  "required": ["path", "repo", "branch"],
  "additionalProperties": false
}"#;

/// Input shape — strict; extra fields rejected via `deny_unknown_fields`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenInput {
    repo: String,
}

/// Output shape returned to the agent on success.
#[derive(Debug, Serialize)]
struct OpenOutput {
    path: String,
    repo: String,
    branch: String,
}

/// `roki_open_worktree` tool implementation.
///
/// Construct one per worker (the issue id is part of the tool's identity).
/// The handler resolves the configured repo via `ghq`, creates the
/// worktree branch via `wt`, registers it in the [`WorktreeRegistry`], and
/// returns the worktree path to the agent.
pub struct OpenWorktreeTool {
    issue: IssueId,
    /// Strict allowlist sourced from the daemon's `[[repos]]` config.
    allowed_repos: Vec<String>,
    ghq: Arc<dyn GhqTool>,
    wt: Arc<dyn WtTool>,
    registry: WorktreeRegistry,
}

impl OpenWorktreeTool {
    /// Construct a new tool bound to `issue` and the supplied dependencies.
    /// `allowed_repos` is the daemon's `[[repos]]` allowlist verbatim.
    pub fn new(
        issue: IssueId,
        allowed_repos: Vec<String>,
        ghq: Arc<dyn GhqTool>,
        wt: Arc<dyn WtTool>,
        registry: WorktreeRegistry,
    ) -> Self {
        Self {
            issue,
            allowed_repos,
            ghq,
            wt,
            registry,
        }
    }
}

#[async_trait]
impl Tool for OpenWorktreeTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn description(&self) -> &'static str {
        TOOL_DESCRIPTION
    }

    fn input_schema(&self) -> &'static str {
        TOOL_INPUT_SCHEMA
    }

    fn output_schema(&self) -> &'static str {
        TOOL_OUTPUT_SCHEMA
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        let parsed: OpenInput =
            serde_json::from_value(input).map_err(|err| ToolError::InvalidInput {
                reason: err.to_string(),
            })?;

        let repo = parsed.repo;
        let issue = self.issue.clone();
        let branch = BranchName::from(&issue);

        // 1. Allowlist enforcement BEFORE any external invocation.
        if !self.allowed_repos.iter().any(|r| r == &repo) {
            return Err(ToolError::RepoNotInAllowlist {
                repo,
                allowed: self.allowed_repos.clone(),
            });
        }

        let repo_id = RepoId::new(repo.clone());

        // 2. Idempotent short-circuit BEFORE ghq/wt invocation.
        if let Some(existing) = self.registry.lookup(&issue, &repo_id) {
            info!(
                target: "tools.roki_open_worktree",
                issue = %issue.as_str(),
                repo = %repo,
                path = %existing.display(),
                "registry hit; returning existing worktree path",
            );
            return Ok(serde_json::to_value(OpenOutput {
                path: existing.to_string_lossy().into_owned(),
                repo,
                branch: branch.as_str().to_string(),
            })
            .expect("OpenOutput serialisation cannot fail"));
        }

        // 3. Resolve / clone the repo via ghq.
        let repo_path = self.ghq.ensure_cloned(&repo).await.map_err(|source| {
            warn!(
                target: "tools.roki_open_worktree",
                issue = %issue.as_str(),
                repo = %repo,
                error = %source,
                "ghq.ensure_cloned failed",
            );
            ToolError::GhqResolutionFailed {
                repo: repo.clone(),
                reason: source.to_string(),
            }
        })?;

        // 4. Create the worktree branch via wt.
        let worktree_path = self
            .wt
            .switch_create(&repo_path, branch.as_str())
            .await
            .map_err(|source| {
                warn!(
                    target: "tools.roki_open_worktree",
                    issue = %issue.as_str(),
                    repo = %repo,
                    branch = %branch.as_str(),
                    error = %source,
                    "wt.switch_create failed",
                );
                ToolError::WorktreeCreationFailed {
                    repo: repo.clone(),
                    branch: branch.as_str().to_string(),
                    reason: source.to_string(),
                }
            })?;

        // 5. Register the worktree under the issue. `register` is itself
        // idempotent so a race where two concurrent tool calls both miss the
        // initial lookup still ends up with one entry and one path.
        let registered_path = self.registry.register(
            issue.clone(),
            repo_id,
            branch.clone(),
            worktree_path.clone(),
        );

        info!(
            target: "tools.roki_open_worktree",
            issue = %issue.as_str(),
            repo = %repo,
            branch = %branch.as_str(),
            path = %registered_path.display(),
            "worktree opened",
        );

        Ok(serde_json::to_value(OpenOutput {
            path: registered_path.to_string_lossy().into_owned(),
            repo,
            branch: branch.as_str().to_string(),
        })
        .expect("OpenOutput serialisation cannot fail"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{GhqError, WtError};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex as StdMutex;

    struct MockGhq {
        repo_path: PathBuf,
        ensure_calls: StdMutex<Vec<String>>,
        force_error: StdMutex<Option<GhqError>>,
    }

    #[async_trait]
    impl GhqTool for MockGhq {
        async fn list_path(&self, _full: &str) -> Result<Option<PathBuf>, GhqError> {
            Ok(Some(self.repo_path.clone()))
        }

        async fn ensure_cloned(&self, full: &str) -> Result<PathBuf, GhqError> {
            self.ensure_calls.lock().unwrap().push(full.to_string());
            if let Some(err) = self.force_error.lock().unwrap().take() {
                return Err(err);
            }
            Ok(self.repo_path.clone())
        }
    }

    struct MockWt {
        switch_calls: StdMutex<Vec<(PathBuf, String)>>,
        force_error: StdMutex<Option<WtError>>,
    }

    #[async_trait]
    impl WtTool for MockWt {
        async fn switch_create(&self, repo_path: &Path, branch: &str) -> Result<PathBuf, WtError> {
            self.switch_calls
                .lock()
                .unwrap()
                .push((repo_path.to_path_buf(), branch.to_string()));
            if let Some(err) = self.force_error.lock().unwrap().take() {
                return Err(err);
            }
            crate::tools::wt::worktree_path_for(repo_path, branch)
        }

        async fn remove(&self, _worktree_path: &Path) -> Result<(), WtError> {
            Ok(())
        }
    }

    fn build_tool(
        issue: &str,
        allowed: &[&str],
        ghq: Arc<MockGhq>,
        wt: Arc<MockWt>,
        registry: WorktreeRegistry,
    ) -> OpenWorktreeTool {
        OpenWorktreeTool::new(
            IssueId::new(issue),
            allowed.iter().map(|s| (*s).to_string()).collect(),
            ghq,
            wt,
            registry,
        )
    }

    #[tokio::test]
    async fn rejects_unallowlisted_repo() {
        let ghq = Arc::new(MockGhq {
            repo_path: PathBuf::from("/tmp/parent/core"),
            ensure_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let wt = Arc::new(MockWt {
            switch_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let registry = WorktreeRegistry::new();
        let tool = build_tool(
            "ENG-1",
            &["owner/core"],
            Arc::clone(&ghq),
            Arc::clone(&wt),
            registry.clone(),
        );

        let err = tool
            .call(serde_json::json!({"repo": "evil/repo"}))
            .await
            .expect_err("non-allowlisted repo must be refused");
        match err {
            ToolError::RepoNotInAllowlist { repo, allowed } => {
                assert_eq!(repo, "evil/repo");
                assert_eq!(allowed, vec!["owner/core".to_string()]);
            }
            other => panic!("expected RepoNotInAllowlist, got {other:?}"),
        }

        // No external calls happen on rejection.
        assert!(ghq.ensure_calls.lock().unwrap().is_empty());
        assert!(wt.switch_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn happy_path_resolves_via_ghq_then_creates_worktree() {
        let parent = tempfile::TempDir::new().unwrap();
        let repo_path = parent.path().join("core");
        std::fs::create_dir_all(&repo_path).unwrap();

        let ghq = Arc::new(MockGhq {
            repo_path: repo_path.clone(),
            ensure_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let wt = Arc::new(MockWt {
            switch_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let registry = WorktreeRegistry::new();
        let tool = build_tool(
            "ENG-1",
            &["owner/core"],
            Arc::clone(&ghq),
            Arc::clone(&wt),
            registry.clone(),
        );

        let out = tool
            .call(serde_json::json!({"repo": "owner/core"}))
            .await
            .expect("call must succeed");
        assert_eq!(out["repo"], "owner/core");
        assert_eq!(out["branch"], "ENG-1");
        let returned_path = out["path"].as_str().unwrap();
        assert_eq!(
            returned_path,
            parent.path().join("core.ENG-1").to_str().unwrap(),
        );

        assert_eq!(ghq.ensure_calls.lock().unwrap().len(), 1);
        assert_eq!(wt.switch_calls.lock().unwrap().len(), 1);
        let (called_repo_path, called_branch) = wt.switch_calls.lock().unwrap()[0].clone();
        assert_eq!(called_repo_path, repo_path);
        assert_eq!(called_branch, "ENG-1");

        // Registry now carries one entry for the issue.
        let entries = registry.list_for_issue(&IssueId::new("ENG-1"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].repo, RepoId::new("owner/core"));
    }

    #[tokio::test]
    async fn idempotent_on_repeat_call() {
        let parent = tempfile::TempDir::new().unwrap();
        let repo_path = parent.path().join("core");
        std::fs::create_dir_all(&repo_path).unwrap();

        let ghq = Arc::new(MockGhq {
            repo_path: repo_path.clone(),
            ensure_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let wt = Arc::new(MockWt {
            switch_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let registry = WorktreeRegistry::new();
        let tool = build_tool(
            "ENG-1",
            &["owner/core"],
            Arc::clone(&ghq),
            Arc::clone(&wt),
            registry.clone(),
        );

        let first = tool
            .call(serde_json::json!({"repo": "owner/core"}))
            .await
            .unwrap();
        let second = tool
            .call(serde_json::json!({"repo": "owner/core"}))
            .await
            .unwrap();
        assert_eq!(first["path"], second["path"]);
        // ghq + wt invoked exactly once across both calls — the registry
        // short-circuits the second call before reaching either.
        assert_eq!(ghq.ensure_calls.lock().unwrap().len(), 1);
        assert_eq!(wt.switch_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn surfaces_ghq_failure_typed() {
        let ghq = Arc::new(MockGhq {
            repo_path: PathBuf::from("/tmp/never"),
            ensure_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(Some(GhqError::NotFound {
                message: "ghq missing".to_string(),
            })),
        });
        let wt = Arc::new(MockWt {
            switch_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let registry = WorktreeRegistry::new();
        let tool = build_tool(
            "ENG-1",
            &["owner/core"],
            Arc::clone(&ghq),
            Arc::clone(&wt),
            registry,
        );

        let err = tool
            .call(serde_json::json!({"repo": "owner/core"}))
            .await
            .expect_err("ghq failure must surface");
        match err {
            ToolError::GhqResolutionFailed { repo, .. } => {
                assert_eq!(repo, "owner/core");
            }
            other => panic!("expected GhqResolutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn surfaces_wt_failure_typed() {
        let parent = tempfile::TempDir::new().unwrap();
        let repo_path = parent.path().join("core");
        std::fs::create_dir_all(&repo_path).unwrap();

        let ghq = Arc::new(MockGhq {
            repo_path: repo_path.clone(),
            ensure_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let wt = Arc::new(MockWt {
            switch_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(Some(WtError::NonZeroExit {
                message: "wt: branch in use".to_string(),
            })),
        });
        let registry = WorktreeRegistry::new();
        let tool = build_tool(
            "ENG-1",
            &["owner/core"],
            Arc::clone(&ghq),
            Arc::clone(&wt),
            registry,
        );

        let err = tool
            .call(serde_json::json!({"repo": "owner/core"}))
            .await
            .expect_err("wt failure must surface");
        match err {
            ToolError::WorktreeCreationFailed {
                repo,
                branch,
                reason,
            } => {
                assert_eq!(repo, "owner/core");
                assert_eq!(branch, "ENG-1");
                assert!(reason.contains("branch in use"));
            }
            other => panic!("expected WorktreeCreationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_extra_input_fields() {
        let ghq = Arc::new(MockGhq {
            repo_path: PathBuf::from("/tmp/p"),
            ensure_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let wt = Arc::new(MockWt {
            switch_calls: StdMutex::new(Vec::new()),
            force_error: StdMutex::new(None),
        });
        let registry = WorktreeRegistry::new();
        let tool = build_tool(
            "ENG-1",
            &["owner/core"],
            Arc::clone(&ghq),
            Arc::clone(&wt),
            registry,
        );

        let err = tool
            .call(serde_json::json!({"repo": "owner/core", "branch": "main"}))
            .await
            .expect_err("extra fields must be refused");
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    #[test]
    fn description_matches_verbatim() {
        // The reviewer greps for this exact string.
        assert_eq!(
            TOOL_DESCRIPTION,
            "Open a git worktree for the current Linear issue in one of the configured repos. The daemon resolves the repo via ghq, creates a worktree branch named after the issue id via wt, and returns the absolute path. Idempotent — calling twice with the same repo returns the same path. Use this once per repo you intend to modify; cross-repo tickets call this multiple times.",
        );
    }
}
