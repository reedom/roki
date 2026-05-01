//! Integration test for task 3.4: wire the tool registry into the engine
//! adapter at worker launch.
//!
//! Drives a fake `claude` binary through a `linear_graphql` tool-use event
//! while a `wiremock` stub stands in for the Linear GraphQL endpoint, and
//! asserts:
//!
//! 1. The tool catalog handed to each worker subprocess at launch is composed
//!    from the live [`Registry`] (req 7.1) and includes the built-in
//!    `linear-graphql` proxy.
//! 2. Forwarding a tool call through the adapter dispatches via the registry
//!    and returns the Linear response unmodified, with the daemon-owned token
//!    absent from input, output, and any error string (req 7.2, 7.4).
//! 3. The daemon-owned API token never reaches the worker subprocess: it is
//!    not present in the prelude envelope and is not present in any
//!    [`SupervisedEvent`] emitted to the orchestrator.

use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::sync::OnceLock;

use roki_daemon::config::SecretString;
use roki_daemon::engine::claude::{ClaudeEngineAdapter, SupervisedEvent, WorkerContext};
use roki_daemon::engine::policy::EnginePolicy;
use roki_daemon::engine::stream::EngineLifecycleEvent;
use roki_daemon::orchestrator::state::{CorrelationId, IssueId};
use roki_daemon::permissions::{PermissionMode, PermissionSource, ResolvedPermission};
use roki_daemon::tools::linear_graphql::LinearGraphqlTool;
use roki_daemon::tools::{InMemoryRegistry, NoopRateLimit, Registry};
use roki_daemon::workflow::{ElicitationsMode, SandboxMode};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "lin_api_task_3_4_super_secret_value";

/// Memoise the `fake_claude` example binary build so each test in this file
/// compiles it at most once per `cargo test` run. Mirrors the helper in
/// `tests/engine_claude.rs`.
fn fake_claude_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = StdCommand::new(&cargo)
            .args(["build", "--example", "fake_claude"])
            .status()
            .expect("must be able to invoke `cargo build --example fake_claude`");
        assert!(
            status.success(),
            "`cargo build --example fake_claude` failed with {status:?}",
        );

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest
            .parent()
            .and_then(|p| p.parent())
            .expect("CARGO_MANIFEST_DIR must have a workspace ancestor")
            .to_path_buf();
        let bin = workspace
            .join("target")
            .join("debug")
            .join("examples")
            .join(if cfg!(windows) {
                "fake_claude.exe"
            } else {
                "fake_claude"
            });
        assert!(
            bin.exists(),
            "fake_claude binary missing at {}",
            bin.display(),
        );
        bin
    })
}

fn allowlist_permission() -> ResolvedPermission {
    ResolvedPermission {
        mode: PermissionMode::Allowlist {
            settings_path: PathBuf::from("/etc/roki/settings.json"),
        },
        sandbox: SandboxMode::WorkspaceWrite,
        elicitations: ElicitationsMode::Reject,
        mode_source: PermissionSource::Operator,
    }
}

fn worker_context(workspace: PathBuf) -> WorkerContext {
    WorkerContext {
        issue: IssueId::new("ENG-tools-1"),
        correlation_id: CorrelationId::from_uuid(Uuid::nil()),
        workspace_dir: workspace,
        prompt: "exercise the tool registry".to_owned(),
        // Intentionally empty: the adapter must populate the catalog from the
        // attached registry at launch (req 7.1).
        tool_catalog: Vec::new(),
        permission: allowlist_permission(),
        policy: EnginePolicy::default(),
        additional_context: None,
    }
}

#[tokio::test]
async fn registry_catalog_reaches_subprocess_without_token_leak() {
    // ---- Stub Linear GraphQL endpoint -------------------------------------
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("authorization", TEST_TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "viewer": { "id": "viewer-task-3-4" } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    // ---- Build a real registry containing the linear_graphql tool ---------
    let tool = LinearGraphqlTool::new(
        format!("{}/", server.uri()),
        SecretString::new(TEST_TOKEN),
        Arc::new(NoopRateLimit),
    )
    .expect("tool must build");
    let registry = Arc::new(InMemoryRegistry::new());
    registry.register(Arc::new(tool)).expect("register tool");

    // ---- Adapter launches the fake claude with the registry attached -----
    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join(".fake_claude_mode"), "tool_call").unwrap();
    let capture_path = workspace.path().join("captured-prelude.txt");
    std::fs::write(
        workspace.path().join(".fake_claude_capture"),
        capture_path.to_str().expect("utf-8 path"),
    )
    .unwrap();

    let adapter = ClaudeEngineAdapter::with_binary(fake_claude_path().clone())
        .with_registry(registry.clone());

    let ctx = worker_context(workspace.path().to_path_buf());

    let (tx, mut rx) = mpsc::channel(64);
    let _outcome = adapter.launch(ctx, tx).await.expect("launch must spawn");

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }

    // The fake emitted a `tool_use` event for `linear-graphql`; the
    // supervisor must have surfaced it as a typed `ToolCall` lifecycle event.
    let saw_tool_call = events.iter().any(|e| {
        matches!(
            e,
            SupervisedEvent::Lifecycle(EngineLifecycleEvent::ToolCall { name })
                if name == "linear-graphql"
        )
    });
    assert!(
        saw_tool_call,
        "expected a ToolCall lifecycle event for linear-graphql, got {events:?}",
    );

    // Token must not appear in any event the supervisor surfaced.
    let rendered_events = format!("{events:?}");
    assert!(
        !rendered_events.contains(TEST_TOKEN),
        "supervisor event stream leaked the API token: {rendered_events}",
    );

    // ---- The prelude must carry the tool catalog (req 7.1) ---------------
    let captured = std::fs::read_to_string(&capture_path)
        .unwrap_or_else(|err| panic!("captured prelude must exist at {capture_path:?}: {err}"));
    assert!(
        captured.contains("linear-graphql"),
        "prelude must list the linear-graphql tool, got:\n{captured}",
    );
    // Token must not appear in the prelude bytes shipped to the subprocess.
    assert!(
        !captured.contains(TEST_TOKEN),
        "prelude shipped the API token to the subprocess: {captured}",
    );

    // ---- Forwarding a tool call through the adapter dispatches via the
    // registry and returns the Linear response without the token (req 7.2,
    // 7.4) -----------------------------------------------------------------
    let result = adapter
        .dispatch_tool(
            "linear-graphql",
            json!({
                "query": "query Me { viewer { id } }",
                "variables": {}
            }),
        )
        .await
        .expect("dispatch must succeed");

    assert_eq!(
        result,
        json!({ "data": { "viewer": { "id": "viewer-task-3-4" } } })
    );
    let rendered_result = serde_json::to_string(&result).unwrap();
    assert!(
        !rendered_result.contains(TEST_TOKEN),
        "tool response leaked the API token: {rendered_result}",
    );
}

#[tokio::test]
async fn dispatch_unknown_tool_returns_redaction_safe_error() {
    // The token must be invisible to error consumers as well (req 7.4). We
    // attach a registry that contains the linear_graphql tool — so the
    // SecretString is held by the adapter's registry — and then dispatch a
    // bogus name to trigger an UnknownTool error. The error string must not
    // accidentally embed any registry-held secrets.
    let tool = LinearGraphqlTool::new(
        "https://api.linear.app/graphql",
        SecretString::new(TEST_TOKEN),
        Arc::new(NoopRateLimit),
    )
    .expect("tool must build");
    let registry = Arc::new(InMemoryRegistry::new());
    registry.register(Arc::new(tool)).expect("register tool");

    let adapter = ClaudeEngineAdapter::new().with_registry(registry);

    let err = adapter
        .dispatch_tool("does-not-exist", json!({}))
        .await
        .unwrap_err();

    let rendered = err.to_string();
    assert!(
        !rendered.contains(TEST_TOKEN),
        "error string leaked the API token: {rendered}",
    );
}
