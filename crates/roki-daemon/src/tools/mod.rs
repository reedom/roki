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

pub mod linear_graphql;

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
