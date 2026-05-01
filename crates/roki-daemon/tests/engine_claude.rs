//! Integration tests for the Claude Code subprocess supervisor (task 2.10).
//!
//! Each test drives a fake `claude` binary (see `examples/fake_claude.rs`)
//! through one of the documented observable-completion scenarios from
//! requirements.md §5 and design.md §Engine, and asserts that the
//! supervisor:
//!
//! 1. Spawns the binary with the workspace as cwd, the documented base flags
//!    (`--print --output-format stream-json`), and the resolved permission
//!    flag (`--settings <path>` for allowlist) attached.
//! 2. Streams a `Lifecycle` event for every parsed stream-json line emitted
//!    by the child.
//! 3. Always emits exactly one terminal `Exited(WorkerOutcome)` event whose
//!    payload classifies the termination as `CleanExit`,
//!    `NonCleanExit { code }`, or `Stalled { reason }`.
//! 4. Forwards `WorkerContext::additional_context` verbatim through the
//!    documented prelude envelope (req 13.4).
//!
//! The fake binary takes its scenario from a `.fake_claude_mode` file inside
//! the workspace cwd (rather than an environment variable) so the tests can
//! run under the workspace-wide `unsafe_code = "forbid"` lint without
//! resorting to the `unsafe` env-mutation APIs introduced in edition 2024.

use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::OnceLock;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;

use roki_daemon::engine::claude::{
    ClaudeEngineAdapter, PRELUDE_ADDITIONAL_CONTEXT_KEY, PRELUDE_CLOSE, PRELUDE_OPEN,
    SupervisedEvent, WorkerContext,
};
use roki_daemon::engine::policy::{BackoffPolicy, EnginePolicy, StallReason, WorkerOutcome};
use roki_daemon::engine::stream::EngineLifecycleEvent;
use roki_daemon::orchestrator::state::{CorrelationId, IssueId, RepoId};
use roki_daemon::permissions::{PermissionMode, PermissionSource, ResolvedPermission};
use roki_daemon::workflow::{ElicitationsMode, SandboxMode};

/// Return the path of the compiled `fake_claude` example binary, building it
/// on first call. Memoised in a `OnceLock` so the (slightly slow) cargo
/// invocation runs at most once per `cargo test` run rather than once per
/// integration test.
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

        // Cargo places examples under `<workspace>/target/debug/examples/`.
        // Walk upward from `CARGO_MANIFEST_DIR` to the workspace root, then
        // descend into the canonical example output directory.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // crate dir → crates/ → workspace root.
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

fn allowlist_permission(settings_path: PathBuf) -> ResolvedPermission {
    ResolvedPermission {
        mode: PermissionMode::Allowlist { settings_path },
        sandbox: SandboxMode::WorkspaceWrite,
        elicitations: ElicitationsMode::Reject,
        mode_source: PermissionSource::Operator,
    }
}

/// Materialise a workspace `TempDir` plus the `.fake_claude_mode` selector
/// file the fake binary reads at startup. Returning the `TempDir` keeps it
/// alive for the duration of the test.
fn workspace_for(mode: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join(".fake_claude_mode"), mode).unwrap();
    dir
}

fn worker_context(workspace: PathBuf, mode: &str) -> WorkerContext {
    WorkerContext {
        repo: RepoId::new("repo-it"),
        issue: IssueId::new("ENG-1"),
        correlation_id: CorrelationId::from_uuid(Uuid::nil()),
        workspace_dir: workspace,
        prompt: format!("integration prompt for mode={mode}"),
        tool_catalog: Vec::new(),
        permission: allowlist_permission(PathBuf::from("/etc/roki/settings.json")),
        policy: EnginePolicy::default(),
        additional_context: None,
    }
}

fn assert_exited_event(events: &[SupervisedEvent], expected: WorkerOutcome) {
    let exited: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, SupervisedEvent::Exited(_)))
        .collect();
    assert_eq!(
        exited.len(),
        1,
        "exactly one terminal Exited event must be emitted; got events={events:?}",
    );
    match exited[0] {
        SupervisedEvent::Exited(actual) => assert_eq!(*actual, expected),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn engine_clean_exit() {
    let workspace = workspace_for("clean_exit");
    let adapter = ClaudeEngineAdapter::with_binary(fake_claude_path().clone());

    let ctx = worker_context(workspace.path().to_path_buf(), "clean_exit");

    let (tx, mut rx) = mpsc::channel(64);
    let outcome = adapter.launch(ctx, tx).await.expect("launch must spawn");

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }

    // The fake emits at least the `system/init` line — observable as a
    // `Started` lifecycle event (req 5.2) — before exiting cleanly.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SupervisedEvent::Lifecycle(EngineLifecycleEvent::Started))),
        "expected `Started` lifecycle event from system/init line; got {events:?}",
    );
    assert_exited_event(&events, WorkerOutcome::CleanExit);
    assert_eq!(outcome, WorkerOutcome::CleanExit);
}

#[tokio::test]
async fn engine_non_clean_exit() {
    let workspace = workspace_for("non_clean_exit");
    let adapter = ClaudeEngineAdapter::with_binary(fake_claude_path().clone());

    let ctx = worker_context(workspace.path().to_path_buf(), "non_clean_exit");

    let (tx, mut rx) = mpsc::channel(64);
    let outcome = adapter.launch(ctx, tx).await.expect("launch must spawn");

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }

    // The fake emits exit code 7 (req 5.6 routes any non-zero status to
    // `NonCleanExit`).
    assert_exited_event(&events, WorkerOutcome::NonCleanExit { code: 7 });
    assert_eq!(outcome, WorkerOutcome::NonCleanExit { code: 7 });
}

#[tokio::test]
async fn engine_stall() {
    let workspace = workspace_for("stall");
    let adapter = ClaudeEngineAdapter::with_binary(fake_claude_path().clone());

    let mut ctx = worker_context(workspace.path().to_path_buf(), "stall");
    // Tighten the stall window so the test runs fast. The backoff knobs are
    // unused in this scenario but must be set explicitly because we are
    // overriding the default policy.
    ctx.policy = EnginePolicy {
        turn_budget: 20,
        stall_window: Duration::from_millis(300),
        backoff: BackoffPolicy::default(),
    };

    let (tx, mut rx) = mpsc::channel(64);
    let outcome = adapter.launch(ctx, tx).await.expect("launch must spawn");

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }

    // Req 5.3: stall outcome wins and the subprocess was killed.
    assert_exited_event(
        &events,
        WorkerOutcome::Stalled {
            reason: StallReason::EventInactivity,
        },
    );
    assert_eq!(
        outcome,
        WorkerOutcome::Stalled {
            reason: StallReason::EventInactivity,
        }
    );
}

#[tokio::test]
async fn additional_context_reaches_the_subprocess_prelude_verbatim() {
    // End-to-end form of the unit test: the supervisor pipes the prelude
    // envelope to the child's stdin. The fake claude (in `capture_prelude`
    // mode) writes everything it received on stdin to a tempfile. We then
    // assert the captured bytes contain the verbatim `additional_context`
    // value under the documented stable key.
    let workspace = workspace_for("capture_prelude");
    let capture = workspace.path().join("captured-prelude.txt");
    std::fs::write(
        workspace.path().join(".fake_claude_capture"),
        capture.to_str().expect("workspace path must be utf-8"),
    )
    .unwrap();

    let adapter = ClaudeEngineAdapter::with_binary(fake_claude_path().clone());
    let value = serde_json::json!({"finding": "missing-tests", "severity": "high"});
    let mut ctx = worker_context(workspace.path().to_path_buf(), "capture_prelude");
    ctx.additional_context = Some(value.clone());

    let (tx, mut rx) = mpsc::channel(64);
    let _outcome = adapter.launch(ctx, tx).await.expect("launch must spawn");

    // Drain the channel so the supervisor task fully completes before we
    // read the captured file (the fake writes stdin to disk before exiting).
    while rx.recv().await.is_some() {}

    let captured = std::fs::read_to_string(&capture)
        .unwrap_or_else(|err| panic!("captured prelude must exist at {capture:?}: {err}"));

    assert!(
        captured.contains(PRELUDE_OPEN) && captured.contains(PRELUDE_CLOSE),
        "captured stdin must include both prelude sentinels, got:\n{captured}",
    );
    assert!(
        captured.contains(r#""finding":"missing-tests""#),
        "additional_context must reach the subprocess verbatim, got:\n{captured}",
    );
    assert!(
        captured.contains(PRELUDE_ADDITIONAL_CONTEXT_KEY),
        "captured prelude must reference the documented stable key `{PRELUDE_ADDITIONAL_CONTEXT_KEY}`, got:\n{captured}",
    );
}
