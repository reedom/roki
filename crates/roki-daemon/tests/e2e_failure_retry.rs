//! End-to-end failure-path integration test (task 4.3).
//!
//! This test wires together the same MVP component graph as the happy-path
//! test (4.2) — fake Linear (wiremock), `LinearTracker`, `TrackerBridge`,
//! `Orchestrator`, real `ClaudeEngineAdapter`, and the compiled
//! `fake_claude` example binary — but drives the engine into a repeated
//! `non_clean_exit` outcome so the retry-budget Backoff loop introduced by
//! task 3.7 must exhaust the configured `max_attempts` and land the worker
//! in `TerminalFailure` with the workspace retained.
//!
//! Determinism notes:
//!
//! * The fake Linear server keeps returning the issue in the `started`
//!   (active) bucket; the test never transitions it to `completed` because
//!   the failure path is independent of tracker-driven terminal promotion.
//! * `fake_claude` reads its mode from `.fake_claude_mode` inside the
//!   per-issue workspace cwd; we install that file via a thin wrapper
//!   around the real `ClaudeEngineAdapter` so every launch (including
//!   relaunches after a Backoff window) starts in `non_clean_exit` mode.
//! * The orchestrator runs with a sub-second `backoff_floor` (50ms) and
//!   `max_attempts = 3`, so the full retry trace completes in well under
//!   a second of real time.
//!
//! Requirements: 5.6, 4.5, 8.1.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use roki_daemon::config::SecretString;
// LinearScope removed by 7.1a; this test still imports `ScopeWatch` for the
// tracker shim until 7.1b/c reshape the orchestrator and tracker.
use roki_daemon::engine::claude::ClaudeEngineAdapter;
use roki_daemon::engine::policy::{BackoffPolicy, EnginePolicy, WorkerOutcome};
use roki_daemon::engine::{SupervisedEvent, WorkerContext};
use roki_daemon::orchestrator::core::{EngineLauncher, LaunchError, Orchestrator};
use roki_daemon::orchestrator::events::{EventBus, SubscriberError, TransitionSubscriber};
use roki_daemon::orchestrator::hooks::HookRegistry;
use roki_daemon::orchestrator::read::OrchestratorRead;
use roki_daemon::orchestrator::state::{RepoId, TransitionEvent, WorkerState};
use roki_daemon::orchestrator::tracker_bridge::TrackerBridge;
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tools::NoopRateLimit;
use roki_daemon::tracker::linear::{LinearTracker, LinearTrackerConfig, ScopeWatch};
use roki_daemon::tracker::model::NormalizedIssue;
use serde_json::{Value, json};

mod common;
use crate::common::MockWt;
use roki_daemon::session::SessionManager;
use roki_daemon::worktrees::WorktreeRegistry;

const TEST_TOKEN: &str = "lin_e2e_failure_retry_token";
const TEST_REPO: &str = "core";
const TEST_ISSUE: &str = "ENG-9";

/// Locate the compiled `fake_claude` example binary, building it on first
/// call. Memoised so the build invocation runs at most once per `cargo test`
/// run.
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
        let workspace_root = manifest
            .parent()
            .and_then(|p| p.parent())
            .expect("CARGO_MANIFEST_DIR must have a workspace ancestor")
            .to_path_buf();
        let bin = workspace_root
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

/// GraphQL response that surfaces the test issue in the `started`
/// (active) lifecycle bucket. The fake Linear keeps serving this payload
/// for the entire test — failure-path semantics do not depend on a
/// tracker-driven terminal state.
fn started_payload() -> Value {
    json!({
        "data": {
            "issues": {
                "nodes": [
                    {
                        "id": "uuid-fail-1",
                        "identifier": TEST_ISSUE,
                        "title": "Failure path",
                        "description": "drive the orchestrator's retry budget to exhaustion",
                        "state": { "type": "started", "name": "In Progress" },
                        "labels": { "nodes": [] },
                        "team": { "key": "ENG" }
                    }
                ]
            }
        }
    })
}

/// Adapter wrapper that installs `.fake_claude_mode = "non_clean_exit"`
/// inside the supervisor-supplied workspace cwd before every launch and
/// then delegates to the real [`ClaudeEngineAdapter`]. The mode file is
/// idempotently rewritten on each launch so the wrapper is safe across
/// the retry-budget Backoff loop.
struct NonCleanExitLauncher {
    adapter: ClaudeEngineAdapter,
}

impl NonCleanExitLauncher {
    fn new(adapter: ClaudeEngineAdapter) -> Self {
        Self { adapter }
    }

    fn install_mode_file(workspace_dir: &Path) -> Result<(), LaunchError> {
        std::fs::write(workspace_dir.join(".fake_claude_mode"), "non_clean_exit")
            .map_err(|err| LaunchError::Engine(format!("install mode file: {err}")))
    }
}

#[async_trait]
impl EngineLauncher for NonCleanExitLauncher {
    async fn launch(
        &self,
        ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError> {
        Self::install_mode_file(&ctx.workspace_dir)?;
        self.adapter
            .launch(ctx, events)
            .await
            .map_err(|err| LaunchError::Engine(err.to_string()))
    }
}

/// Records every transition event the orchestrator publishes, in order.
struct RecordingObserver {
    log: Arc<Mutex<Vec<TransitionEvent>>>,
}

#[async_trait]
impl TransitionSubscriber for RecordingObserver {
    fn id(&self) -> &str {
        "e2e-failure-recorder"
    }

    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        self.log.lock().await.push(event.clone());
        Ok(())
    }
}

/// Poll `cond` every 5ms until it returns `true` or `timeout` elapses.
async fn await_cond<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    while !cond() {
        if timeout <= start.elapsed() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

/// Sub-second policy that mirrors the unit-test override used by
/// `tests/orchestrator_core.rs` task-3.7 fixtures: a 50ms backoff floor,
/// fixed (non-growing) per-attempt delay, and the configured retry budget.
/// The production default is `BACKOFF_FLOOR = 10s`; overriding to 50ms
/// keeps the entire retry trace under one second of real time so the test
/// is fast and deterministic.
fn fast_retry_policy(max_attempts: u32) -> EnginePolicy {
    EnginePolicy {
        backoff: BackoffPolicy {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(50),
            multiplier: 1.0,
        },
        backoff_floor: Duration::from_millis(50),
        max_attempts,
        ..EnginePolicy::default()
    }
}

/// The end-to-end retry-budget exhaustion test pinned by tasks.md task 4.3.
#[tokio::test]
#[tracing_test::traced_test]
async fn e2e_failure_path_retry_budget_exhaustion() {
    // ---- Fake Linear --------------------------------------------------
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(started_payload()))
        .mount(&server)
        .await;

    // ---- Session tempdir wiring -------------------------------------
    // Post-7.1d: the worker's CWD is a session tempdir managed by
    // `SessionManager`; the agent itself decides which configured repos to
    // operate in via `roki_open_worktree`. For this retry-trace test, no
    // worktrees are opened — the engine launcher writes its mode file
    // straight into the session tempdir.
    let parent = TempDir::new().expect("session tempdir");
    let session_root = parent.path().join("sessions");
    let parent_path = parent.path().to_path_buf();
    let session_manager = Arc::new(SessionManager::with_root(session_root.clone()));
    let registry = WorktreeRegistry::new();
    let wt: Arc<dyn roki_daemon::tools::WtTool> = Arc::new(MockWt::default());
    let _parent_keep = parent;

    // ---- Orchestrator wiring -----------------------------------------
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded: Arc<Mutex<Vec<TransitionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    event_bus.register(Arc::new(RecordingObserver {
        log: Arc::clone(&recorded),
    }));

    // Engine launcher: real `ClaudeEngineAdapter` driving the
    // `fake_claude` example binary in `non_clean_exit` mode. Every launch
    // exits with a non-zero status code, which the supervisor reports as
    // `WorkerOutcome::NonCleanExit`; that is the only outcome variant
    // that consumes retry-budget per task 3.7.
    let adapter = ClaudeEngineAdapter::with_binary(fake_claude_path().clone());
    let engine: Arc<dyn EngineLauncher> = Arc::new(NonCleanExitLauncher::new(adapter));

    // Channels:
    // * polling_tx is fed by the LinearTracker
    // * webhook_rx is left empty for this test
    // * inbox carries deduped events into the orchestrator
    let (polling_tx, polling_rx) = mpsc::channel::<NormalizedIssue>(16);
    let (_webhook_tx, webhook_rx) = mpsc::channel::<NormalizedIssue>(16);
    let (inbox_tx, inbox_rx) = mpsc::channel::<NormalizedIssue>(16);

    let bridge = TrackerBridge::new(polling_rx, webhook_rx, inbox_tx);
    let bridge_handle = tokio::spawn(bridge.run());

    // Sub-second backoff_floor + max_attempts=3 keeps the trace finite
    // and fast. `with_engine_policy` is the only production-shipped
    // hookpoint that lets a test override these knobs without touching
    // the daemon's main wiring code.
    let orchestrator = Orchestrator::new(
        Arc::clone(&session_manager),
        registry.clone(),
        Arc::clone(&wt),
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        inbox_rx,
    )
    .with_engine_policy(fast_retry_policy(3));
    let read_handle = orchestrator.read_handle();
    let orch_handle = tokio::spawn(async move { orchestrator.run().await });

    // ---- Linear tracker ----------------------------------------------
    let endpoint = format!("{}/graphql", server.uri());
    let tracker_config = LinearTrackerConfig {
        endpoint,
        cadence: Duration::from_millis(50),
        scopes: vec![ScopeWatch {
            repo: RepoId::new(TEST_REPO),
        }],
        token: SecretString::new(TEST_TOKEN),
        rate_limit: Arc::new(NoopRateLimit),
    };
    let tracker = LinearTracker::new(tracker_config);
    let (tracker_shutdown_tx, tracker_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let tracker_handle =
        tokio::spawn(async move { tracker.run(polling_tx, tracker_shutdown_rx).await });

    // ---- Drive: wait for TerminalFailure -----------------------------
    let reached_failure = await_cond(Duration::from_secs(20), || {
        let log = recorded.try_lock();
        match log {
            Ok(entries) => entries
                .iter()
                .any(|ev| ev.next == WorkerState::TerminalFailure),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached_failure,
        "actor must reach TerminalFailure within timeout (recorded so far: {:?})",
        recorded.lock().await,
    );

    // ---- Assertions on the published transition log ------------------
    let final_log = recorded.lock().await.clone();
    let observed_pairs: Vec<(WorkerState, WorkerState)> =
        final_log.iter().map(|ev| (ev.previous, ev.next)).collect();

    // Task 3.7 documented retry trace for `max_attempts = 3`:
    //   Discovered → Queued
    //   Queued → Active        (attempt 1 begins)
    //   Active → Backoff       (attempt 1 failed, budget remains)
    //   Backoff → Active       (attempt 2 begins)
    //   Active → Backoff       (attempt 2 failed, budget remains)
    //   Backoff → Active       (attempt 3 begins)
    //   Active → TerminalFailure (attempt 3 failed, budget exhausted)
    let expected_pairs = vec![
        (WorkerState::Discovered, WorkerState::Queued),
        (WorkerState::Queued, WorkerState::Active),
        (WorkerState::Active, WorkerState::Backoff),
        (WorkerState::Backoff, WorkerState::Active),
        (WorkerState::Active, WorkerState::Backoff),
        (WorkerState::Backoff, WorkerState::Active),
        (WorkerState::Active, WorkerState::TerminalFailure),
    ];
    assert_eq!(
        observed_pairs, expected_pairs,
        "retry-trace must match the documented Active → Backoff → Active → … → TerminalFailure sequence for max_attempts=3",
    );

    // Every transition must carry the same correlation id (one launch
    // identity per actor, threaded through every committed transition).
    let first_correlation = final_log[0].correlation_id;
    for ev in &final_log {
        assert_eq!(
            ev.correlation_id, first_correlation,
            "correlation id must be stable across the actor's transitions; got mismatch at {ev:?}",
        );
        assert_eq!(ev.issue.as_str(), TEST_ISSUE);
    }

    // ---- Session-tempdir retention (Requirement 4.5, design decision #6)
    // The session tempdir must remain on disk after TerminalFailure so an
    // operator can inspect the failed run.
    let expected_session = session_root.join(TEST_ISSUE);
    assert!(
        expected_session.is_dir(),
        "session tempdir must be retained after TerminalFailure; expected {expected_session:?}",
    );
    // Suppress the unused-binding warning when this assertion is the only
    // post-loop check that touches `parent_path`.
    let _ = &parent_path;

    // ---- Orchestrator read snapshot ----------------------------------
    let snapshot = read_handle.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    assert_eq!(snapshot.issues[0].issue.as_str(), TEST_ISSUE);
    assert_eq!(snapshot.issues[0].state, WorkerState::TerminalFailure);

    // ---- Failure log assertions (Requirement 8.1 observability) ------
    // The retry-budget escalation log entry produced by
    // `orchestrator::core` must include the documented diagnostic fields.
    assert!(
        logs_contain("retry budget exhausted"),
        "retry-budget escalation log entry must be emitted",
    );
    assert!(
        logs_contain("final_attempt=3"),
        "TerminalFailure log must record the final attempt count",
    );
    assert!(
        logs_contain("max_attempts=3"),
        "TerminalFailure log must record the configured max_attempts",
    );
    assert!(
        logs_contain("last_outcome_reason=\"non_clean_exit\""),
        "TerminalFailure log must record the last outcome reason",
    );

    // ---- Tear down ---------------------------------------------------
    let _ = tracker_shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), tracker_handle).await;

    drop(_webhook_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), bridge_handle).await;

    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(5), orch_handle).await;
}
