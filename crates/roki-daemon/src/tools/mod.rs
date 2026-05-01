//! Agent tool registry and built-in tools.
//!
//! The registry is the daemon's audited surface for agent-callable tools. Every
//! entry declares a stable kebab-case name plus JSON-Schema strings for input
//! and output, and the registry exposes a serialised catalog the engine adapter
//! ships to each worker subprocess at launch (req 7.1, 7.5).
//!
//! `linear_graphql` is the only tool implemented in this task; downstream specs
//! may register additional read-only tools without modifying the trait
//! contract.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod ghq;
pub mod linear_graphql;
pub mod roki_open_worktree;
pub mod wt;

pub use ghq::{GhqError, GhqTool, RealGhq};
pub use roki_open_worktree::OpenWorktreeTool;
pub use wt::{RealWt, WorktreePorcelainEntry, WtError, WtTool};

/// Stable surface for an agent-callable tool. Implementations MUST keep
/// `name`, `input_schema`, and `output_schema` immutable for the life of the
/// process so the catalog handed to a worker at launch stays valid.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable kebab-case identifier exposed to the agent.
    fn name(&self) -> &'static str;

    /// Human-readable description of the tool's purpose.
    fn description(&self) -> &'static str;

    /// JSON-Schema document describing accepted inputs (as a string literal).
    fn input_schema(&self) -> &'static str;

    /// JSON-Schema document describing the response shape.
    fn output_schema(&self) -> &'static str;

    /// Invoke the tool with the agent-supplied arguments.
    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, ToolError>;
}

#[cfg(test)]
mod factory_tests {
    //! Unit tests for [`DefaultWorkerToolFactory`] (task 7.1f).

    use super::*;
    use crate::orchestrator::state::IssueId;
    use crate::worktrees::WorktreeRegistry;
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};

    struct StubGhq;
    #[async_trait]
    impl ghq::GhqTool for StubGhq {
        async fn list_path(&self, _full: &str) -> Result<Option<PathBuf>, ghq::GhqError> {
            Ok(None)
        }
        async fn ensure_cloned(&self, _full: &str) -> Result<PathBuf, ghq::GhqError> {
            Ok(PathBuf::new())
        }
    }

    struct StubWt;
    #[async_trait]
    impl wt::WtTool for StubWt {
        async fn switch_create(
            &self,
            _repo_path: &Path,
            _branch: &str,
        ) -> Result<PathBuf, wt::WtError> {
            Ok(PathBuf::new())
        }
        async fn remove(&self, _worktree_path: &Path) -> Result<(), wt::WtError> {
            Ok(())
        }
        async fn list_porcelain(
            &self,
            _repo_path: &Path,
        ) -> Result<Vec<wt::WorktreePorcelainEntry>, wt::WtError> {
            Ok(Vec::new())
        }
    }

    /// Build a stub `linear_graphql` tool for the registry test. The real
    /// `LinearGraphqlTool::new` requires a live `reqwest::Client` builder
    /// which is fine in test env, but we keep the stub minimal to avoid
    /// cross-test interference.
    struct StubLinearGraphql;
    #[async_trait]
    impl Tool for StubLinearGraphql {
        fn name(&self) -> &'static str {
            "linear-graphql"
        }
        fn description(&self) -> &'static str {
            "stub"
        }
        fn input_schema(&self) -> &'static str {
            "{}"
        }
        fn output_schema(&self) -> &'static str {
            "{}"
        }
        async fn call(&self, _input: serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::Value::Null)
        }
    }

    #[test]
    fn per_worker_registry_carries_linear_graphql_and_roki_open_worktree() {
        // Task 7.1f acceptance: the per-worker registry produced by
        // `DefaultWorkerToolFactory::build_for_issue` must carry both
        // tools every worker is expected to see, named exactly per the
        // SPEC.md §7 contract.
        let factory = DefaultWorkerToolFactory::new(
            vec![Arc::new(StubLinearGraphql) as Arc<dyn Tool>],
            vec!["owner/core".to_string()],
            Arc::new(StubGhq),
            Arc::new(StubWt),
            WorktreeRegistry::new(),
        );
        let registry = factory.build_for_issue(&IssueId::new("ENG-1"));
        let names: Vec<&str> = registry.catalog().iter().map(|d| d.name).collect();
        assert!(
            names.contains(&"linear-graphql"),
            "per-worker registry must contain linear-graphql; got {names:?}",
        );
        assert!(
            names.contains(&"roki_open_worktree"),
            "per-worker registry must contain roki_open_worktree; got {names:?}",
        );
    }
}

/// Catalog entry for a single registered tool. The catalog is serialised as
/// part of the worker launch payload, so the field names are part of the
/// daemon ↔ worker contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON-Schema for the tool input, parsed from the tool's static string.
    pub input_schema: serde_json::Value,
    /// JSON-Schema for the tool output, parsed from the tool's static string.
    pub output_schema: serde_json::Value,
}

/// Registry contract used by the engine adapter and downstream specs.
#[async_trait]
pub trait Registry: Send + Sync {
    /// Register a tool. Returns [`ToolError::DuplicateName`] if `tool.name()`
    /// is already known.
    fn register(&self, tool: Arc<dyn Tool>) -> Result<(), ToolError>;

    /// Snapshot of every registered tool, suitable for serialising into the
    /// worker launch payload.
    fn catalog(&self) -> Vec<ToolDescriptor>;

    /// Dispatch a tool call by stable name. Returns
    /// [`ToolError::UnknownTool`] when no tool is registered under `name`.
    async fn dispatch(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError>;
}

/// Build a per-worker [`Registry`] populated with every tool the agent
/// for `issue` should see.
///
/// The orchestrator constructs one registry per worker because some tools
/// (notably [`roki_open_worktree::OpenWorktreeTool`]) are bound to a single
/// `IssueId` at construction time — the tool's identity carries the issue
/// the agent is working on so the registry surface is naturally scoped to
/// the worker that owns it.
///
/// Implementations populate the returned [`InMemoryRegistry`] with the
/// daemon-wide tools (e.g. `linear_graphql`) plus a fresh
/// `OpenWorktreeTool` constructed for `issue`. The factory is invoked at
/// `Orchestrator::launch_once` time (task 7.1f); production callers
/// inject [`DefaultWorkerToolFactory`].
pub trait WorkerToolFactory: Send + Sync + 'static {
    /// Produce a registry populated with every tool the worker for `issue`
    /// should see. The returned `Arc<dyn Registry>` is forwarded to the
    /// engine adapter so its catalog snapshot reaches the worker
    /// subprocess and tool dispatch is routed through the same instance.
    fn build_for_issue(&self, issue: &crate::orchestrator::state::IssueId) -> Arc<dyn Registry>;
}

/// Default worker tool factory used by the daemon's bootstrap.
///
/// Composes the per-worker registry from:
///
/// * a single shared `linear_graphql` tool instance (stateless across
///   workers), AND
/// * a fresh `OpenWorktreeTool` bound to the worker's [`IssueId`] and
///   carrying the operator's `[[repos]]` allowlist.
///
/// Tests and downstream specs may register additional read-only tools by
/// extending the `daemon_tools` field at construction time.
pub struct DefaultWorkerToolFactory {
    /// Daemon-wide tools that are stateless across workers (e.g. the
    /// `linear_graphql` proxy). Registered into every per-worker registry.
    daemon_tools: Vec<Arc<dyn Tool>>,
    /// Operator's `[[repos]]` allowlist, propagated to each per-worker
    /// `OpenWorktreeTool`. Strict allowlist enforcement runs against
    /// this slice before any external invocation.
    allowed_repos: Vec<String>,
    /// `ghq` adapter shared across workers; the per-worker
    /// `OpenWorktreeTool` borrows from this `Arc`.
    ghq: Arc<dyn ghq::GhqTool>,
    /// `wt` adapter shared across workers.
    wt: Arc<dyn wt::WtTool>,
    /// Process-wide [`crate::worktrees::WorktreeRegistry`] cloned into
    /// each per-worker tool so the orchestrator's `Cleaning` walk and the
    /// agent's `roki_open_worktree` calls observe a single source of
    /// truth.
    worktree_registry: crate::worktrees::WorktreeRegistry,
}

impl DefaultWorkerToolFactory {
    pub fn new(
        daemon_tools: Vec<Arc<dyn Tool>>,
        allowed_repos: Vec<String>,
        ghq: Arc<dyn ghq::GhqTool>,
        wt: Arc<dyn wt::WtTool>,
        worktree_registry: crate::worktrees::WorktreeRegistry,
    ) -> Self {
        Self {
            daemon_tools,
            allowed_repos,
            ghq,
            wt,
            worktree_registry,
        }
    }
}

impl WorkerToolFactory for DefaultWorkerToolFactory {
    fn build_for_issue(&self, issue: &crate::orchestrator::state::IssueId) -> Arc<dyn Registry> {
        let registry = InMemoryRegistry::new();
        for tool in &self.daemon_tools {
            // Duplicate names would be a bootstrap-time configuration bug;
            // the operator declared two tools with the same `name`. Surface
            // it loudly during construction rather than at runtime.
            registry
                .register(Arc::clone(tool))
                .expect("daemon-wide tool registration must not collide");
        }
        let open = Arc::new(roki_open_worktree::OpenWorktreeTool::new(
            issue.clone(),
            self.allowed_repos.clone(),
            Arc::clone(&self.ghq),
            Arc::clone(&self.wt),
            self.worktree_registry.clone(),
        ));
        registry
            .register(open)
            .expect("OpenWorktreeTool registration must not collide with daemon tools");
        Arc::new(registry)
    }
}

/// In-memory tool registry. Cheap to clone via `Arc` and safe to share between
/// the orchestrator and worker plumbing.
#[derive(Default, Clone)]
pub struct InMemoryRegistry {
    inner: Arc<RwLock<HashMap<&'static str, Arc<dyn Tool>>>>,
}

impl InMemoryRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Registry for InMemoryRegistry {
    fn register(&self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.name();
        let mut guard = self
            .inner
            .write()
            .map_err(|_| ToolError::RegistryPoisoned)?;
        if guard.contains_key(name) {
            return Err(ToolError::DuplicateName {
                name: name.to_string(),
            });
        }
        guard.insert(name, tool);
        Ok(())
    }

    fn catalog(&self) -> Vec<ToolDescriptor> {
        let guard = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut entries: Vec<ToolDescriptor> = guard
            .values()
            .map(|tool| ToolDescriptor {
                name: tool.name(),
                description: tool.description(),
                input_schema: serde_json::from_str(tool.input_schema())
                    .unwrap_or(serde_json::Value::Null),
                output_schema: serde_json::from_str(tool.output_schema())
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect();
        // Stable ordering keeps the worker launch payload deterministic and
        // makes the catalog easy to assert against in tests.
        entries.sort_by_key(|d| d.name);
        entries
    }

    async fn dispatch(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let tool = {
            let guard = self.inner.read().map_err(|_| ToolError::RegistryPoisoned)?;
            guard.get(name).cloned()
        };
        match tool {
            Some(tool) => tool.call(input).await,
            None => Err(ToolError::UnknownTool {
                name: name.to_string(),
            }),
        }
    }
}

/// Structured tool error returned to the agent. Variants intentionally carry
/// only sanitised payloads — the [`linear_graphql`] tool is responsible for
/// scrubbing the daemon-owned API token before any error reaches this enum.
#[derive(Debug, Error)]
pub enum ToolError {
    /// A `linear_graphql` call contained more than one operation definition.
    #[error(
        "MULTIPLE_OPERATIONS: a linear_graphql call must contain exactly one GraphQL operation"
    )]
    MultipleOperations,

    /// Caller-supplied JSON failed schema validation before any side effects.
    #[error("INVALID_INPUT: {reason}")]
    InvalidInput { reason: String },

    /// Linear answered with HTTP 429 or otherwise asked us to back off.
    #[error("RATE_LIMITED: retry_after={retry_after_seconds:?}s")]
    RateLimited { retry_after_seconds: Option<u64> },

    /// Linear answered with a non-2xx, non-429 HTTP status.
    #[error("LINEAR_HTTP_ERROR: status={status}")]
    LinearHttpError { status: u16 },

    /// The HTTP transport failed before a response was received.
    #[error("LINEAR_HTTP_ERROR: {message}")]
    Network { message: String },

    /// Redaction discovered a token leak it could not scrub safely. Treated
    /// as a programmer error: the call is failed loudly rather than risk
    /// returning the raw secret.
    #[error("REDACTION_FAILED")]
    RedactionFailed,

    /// The registry refused to register a duplicate name.
    #[error("DUPLICATE_TOOL: {name}")]
    DuplicateName { name: String },

    /// `dispatch` was called with a name no tool is registered under.
    #[error("UNKNOWN_TOOL: {name}")]
    UnknownTool { name: String },

    /// The internal registry lock was poisoned by a previous panic. Surfaced
    /// rather than re-panicking so the orchestrator can decide how to recover.
    #[error("REGISTRY_POISONED")]
    RegistryPoisoned,

    /// The agent called `roki_open_worktree` with a `repo` that is not in
    /// the operator-configured `[[repos]]` allowlist (task 7.1d locked
    /// decision #3). The agent receives the rejected repo plus the
    /// allowed list so it can recover with a valid choice.
    #[error("REPO_NOT_IN_ALLOWLIST: repo={repo}, allowed={allowed:?}")]
    RepoNotInAllowlist { repo: String, allowed: Vec<String> },

    /// `ghq.ensure_cloned` failed inside `roki_open_worktree`. Surfaced as
    /// a typed tool error so the agent can present the failure to the user
    /// rather than treating it as a generic failure.
    #[error("GHQ_RESOLUTION_FAILED: repo={repo}, reason={reason}")]
    GhqResolutionFailed { repo: String, reason: String },

    /// `wt.switch_create` failed inside `roki_open_worktree` (e.g., the
    /// branch already exists at a conflicting worktree). Carries the
    /// specific `(repo, branch)` pair and the captured failure reason.
    #[error("WORKTREE_CREATION_FAILED: repo={repo}, branch={branch}, reason={reason}")]
    WorktreeCreationFailed {
        repo: String,
        branch: String,
        reason: String,
    },
}

/// Trait shared with the tracker (task 2.5) so the daemon enforces a single
/// view of Linear's rate-limit state. The proxy consults `before_call` before
/// every HTTP request and reports each response via `record_response`.
#[async_trait]
pub trait RateLimitState: Send + Sync {
    /// Return [`Err(RateLimited)`] if the caller MUST defer the request.
    async fn before_call(&self) -> Result<(), RateLimited>;

    /// Update internal state from a Linear response. `retry_after` is the
    /// integer seconds advertised by the `Retry-After` header, when present.
    async fn record_response(&self, status: u16, retry_after: Option<u64>);
}

/// Signal returned by [`RateLimitState::before_call`] to ask the caller to
/// stop and surface a rate-limit error to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    pub retry_after_seconds: Option<u64>,
}

/// Always-allow rate-limit state, used in tests and as a safe default until
/// the tracker (task 2.5) wires the real implementation in.
#[derive(Debug, Default, Clone)]
pub struct NoopRateLimit;

#[async_trait]
impl RateLimitState for NoopRateLimit {
    async fn before_call(&self) -> Result<(), RateLimited> {
        Ok(())
    }

    async fn record_response(&self, _status: u16, _retry_after: Option<u64>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial tool used to exercise the registry contract without pulling
    /// in HTTP plumbing.
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "Returns the input unchanged"
        }
        fn input_schema(&self) -> &'static str {
            r#"{"type":"object","properties":{"value":{"type":"string"}}}"#
        }
        fn output_schema(&self) -> &'static str {
            r#"{"type":"object","properties":{"value":{"type":"string"}}}"#
        }
        async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(input)
        }
    }

    struct OtherTool;

    #[async_trait]
    impl Tool for OtherTool {
        fn name(&self) -> &'static str {
            "other"
        }
        fn description(&self) -> &'static str {
            "stub"
        }
        fn input_schema(&self) -> &'static str {
            r#"{"type":"object"}"#
        }
        fn output_schema(&self) -> &'static str {
            r#"{"type":"object"}"#
        }
        async fn call(&self, _input: serde_json::Value) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn registry_dispatches_registered_tool_by_name() {
        let registry = InMemoryRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();

        let result = registry
            .dispatch("echo", serde_json::json!({"value": "hi"}))
            .await
            .unwrap();

        assert_eq!(result, serde_json::json!({"value": "hi"}));
    }

    #[tokio::test]
    async fn registry_returns_unknown_tool_for_missing_name() {
        let registry = InMemoryRegistry::new();
        let err = registry
            .dispatch("missing", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool { .. }));
    }

    #[test]
    fn registry_rejects_duplicate_registration() {
        let registry = InMemoryRegistry::new();
        registry.register(Arc::new(EchoTool)).unwrap();
        let err = registry.register(Arc::new(EchoTool)).unwrap_err();
        assert!(matches!(err, ToolError::DuplicateName { .. }));
    }

    #[test]
    fn catalog_lists_each_tool_with_parsed_schemas_in_stable_order() {
        let registry = InMemoryRegistry::new();
        registry.register(Arc::new(OtherTool)).unwrap();
        registry.register(Arc::new(EchoTool)).unwrap();

        let catalog = registry.catalog();
        assert_eq!(catalog.len(), 2);

        // Sorted by name.
        assert_eq!(catalog[0].name, "echo");
        assert_eq!(catalog[1].name, "other");

        // Schemas are surfaced as parsed JSON, not the raw string.
        assert!(catalog[0].input_schema.is_object());
        assert!(catalog[0].output_schema.is_object());

        // The descriptor round-trips through serde so the engine adapter
        // can serialise it into the worker launch payload.
        let serialised = serde_json::to_value(&catalog[0]).unwrap();
        assert_eq!(serialised["name"], "echo");
        assert!(serialised["input_schema"].is_object());
    }

    #[tokio::test]
    async fn noop_rate_limit_always_allows_calls() {
        let limiter = NoopRateLimit;
        assert!(limiter.before_call().await.is_ok());
        limiter.record_response(200, None).await;
    }
}
