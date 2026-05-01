//! End-to-end happy-path integration test (task 4.2).
//!
//! This test wires together every MVP component delivered by tasks 3.2,
//! 3.3, 3.5, and 3.6 behind the smallest possible test boundary so the full
//! `Discovered -> Queued -> Active -> AwaitingReview -> TerminalSuccess
//! -> Cleaning` lifecycle is exercised exactly the way the daemon's `main`
//! would wire it in production:
//!
//! * a `wiremock` server stands in for Linear's GraphQL endpoint;
//! * a `LinearTracker` polls that server and emits `NormalizedIssue` events;
//! * a `TrackerBridge` fans the polling stream into the orchestrator inbox
//!   with `(repo, issue, target_state)` dedup;
//! * an `Orchestrator` consumes the inbox and drives a per-`(repo, issue)`
//!   worker actor that launches the `fake_claude` example binary as the
//!   `claude` engine adapter;
//! * a `RecordingObserver` subscribed to the orchestrator's `EventBus`
//!   captures every committed `TransitionEvent` so the test can assert the
//!   exact published sequence and correlation-id stability.
//!
//! Determinism notes:
//!
//! * The fake Linear server returns the issue first as `started` and then,
//!   after one delivery, as `completed`. The bridge collapses repeated polls
//!   so the orchestrator sees exactly two distinct tracker events for the
//!   key.
//! * `fake_claude` defaults to its `clean_exit` mode when no
//!   `.fake_claude_mode` file is present — exactly the engine outcome the
//!   orchestrator promotes to `Active -> AwaitingReview`. We therefore do
//!   not need a new fake-claude mode for the happy path.
//! * Every wait uses a real signal (state-snapshot poll inside a finite
//!   `tokio::time::timeout`); there are no fixed-duration sleeps that decide
//!   ordering.
//!
//! Requirements: 1.1, 4.3, 8.2, 10.3, 13.2.

use std::path::PathBuf;
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
use roki_daemon::config::repos::LinearScope;
use roki_daemon::engine::claude::ClaudeEngineAdapter;
use roki_daemon::engine::policy::WorkerOutcome;
use roki_daemon::engine::{SupervisedEvent, WorkerContext};
use roki_daemon::orchestrator::core::{EngineLauncher, LaunchError, Orchestrator};
use roki_daemon::orchestrator::events::{EventBus, SubscriberError, TransitionSubscriber};
use roki_daemon::orchestrator::hooks::HookRegistry;
use roki_daemon::orchestrator::read::OrchestratorRead;
use roki_daemon::orchestrator::state::{CorrelationId, RepoId, TransitionEvent, WorkerState};
use roki_daemon::orchestrator::tracker_bridge::TrackerBridge;
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tools::NoopRateLimit;
use roki_daemon::tracker::linear::{LinearTracker, LinearTrackerConfig, ScopeWatch};
use roki_daemon::tracker::model::NormalizedIssue;
use serde_json::{Value, json};

mod common;
use crate::common::{build_workspace_manager, expected_worktree_path};

const TEST_TOKEN: &str = "lin_e2e_happy_path_token";
const TEST_REPO: &str = "core";
const TEST_ISSUE: &str = "ENG-1";

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
/// (active) lifecycle bucket. Returned by the fake Linear server until the
/// test redirects polls to the terminal payload.
fn started_payload() -> Value {
    json!({
        "data": {
            "issues": {
                "nodes": [
                    {
                        "id": "uuid-1",
                        "identifier": TEST_ISSUE,
                        "title": "Happy path",
                        "description": "drive the orchestrator end-to-end",
                        "state": { "type": "started", "name": "In Progress" },
                        "labels": { "nodes": [] },
                        "team": { "key": "ENG" }
                    }
                ]
            }
        }
    })
}

/// GraphQL response that surfaces the test issue in the `completed`
/// (terminal) lifecycle bucket so the orchestrator promotes the actor
/// through `AwaitingReview -> TerminalSuccess -> Cleaning`.
fn completed_payload() -> Value {
    json!({
        "data": {
            "issues": {
                "nodes": [
                    {
                        "id": "uuid-1",
                        "identifier": TEST_ISSUE,
                        "title": "Happy path",
                        "description": "drive the orchestrator end-to-end",
                        "state": { "type": "completed", "name": "Done" },
                        "labels": { "nodes": [] },
                        "team": { "key": "ENG" }
                    }
                ]
            }
        }
    })
}

/// Adapter wrapper: bridge the `ClaudeEngineAdapter` into the
/// orchestrator-side `EngineLauncher` trait. The test boundary forbids
/// changes to production source, so the wrapper lives here.
struct ClaudeEngineLauncher {
    adapter: ClaudeEngineAdapter,
}

impl ClaudeEngineLauncher {
    fn new(adapter: ClaudeEngineAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl EngineLauncher for ClaudeEngineLauncher {
    async fn launch(
        &self,
        ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError> {
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
        "e2e-recorder"
    }

    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        self.log.lock().await.push(event.clone());
        Ok(())
    }
}

/// Poll `cond` every 5ms until it returns `true` or `timeout` elapses.
/// Returns `true` iff the condition fired before the deadline. Used in
/// place of fixed sleeps so that test pacing follows real async progress.
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

/// The end-to-end happy-path test pinned by tasks.md task 4.2.
#[tokio::test]
async fn e2e_happy_path_drives_full_lifecycle() {
    // ---- Fake Linear ---------------------------------------------------
    // The fake Linear initially returns the issue as `started`. After the
    // test confirms the workspace was created, the mocks are reset and
    // a `completed` mock is mounted so the next poll drives the actor
    // through `AwaitingReview -> TerminalSuccess -> Cleaning`. This
    // staged sequencing lets the test deterministically observe the
    // workspace's existence between activation and cleanup without
    // racing the polling loop.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(started_payload()))
        .mount(&server)
        .await;

    // ---- Workspace ----------------------------------------------------
    // Each per-issue workspace is created under this root. `fake_claude`
    // defaults to `clean_exit` when no `.fake_claude_mode` file is
    // present, which is exactly the outcome we need to drive
    // `Active -> AwaitingReview`.
    let parent = TempDir::new().expect("workspace tempdir");
    let parent_path = parent.path().to_path_buf();
    let (manager, _parent_keep, _wt, _ghq) =
        build_workspace_manager(parent, &[(TEST_REPO, "owner/core", "core")]);
    let workspace_manager = Arc::new(manager);

    // ---- Orchestrator wiring -----------------------------------------
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded: Arc<Mutex<Vec<TransitionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    event_bus.register(Arc::new(RecordingObserver {
        log: Arc::clone(&recorded),
    }));

    // Build the engine launcher that drives the real subprocess
    // supervisor against the `fake_claude` example.
    let adapter = ClaudeEngineAdapter::with_binary(fake_claude_path().clone());
    let engine: Arc<dyn EngineLauncher> = Arc::new(ClaudeEngineLauncher::new(adapter));

    // Channels:
    // * polling_tx is fed by the LinearTracker
    // * webhook_rx is left empty for this test (the polling path is
    //   sufficient to drive the lifecycle)
    // * inbox carries deduped events into the orchestrator
    let (polling_tx, polling_rx) = mpsc::channel::<NormalizedIssue>(16);
    let (_webhook_tx, webhook_rx) = mpsc::channel::<NormalizedIssue>(16);
    let (inbox_tx, inbox_rx) = mpsc::channel::<NormalizedIssue>(16);

    let bridge = TrackerBridge::new(polling_rx, webhook_rx, inbox_tx);
    let bridge_handle = tokio::spawn(bridge.run());

    let orchestrator = Orchestrator::new(
        Arc::clone(&workspace_manager) as Arc<_>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        inbox_rx,
    );
    let read_handle = orchestrator.read_handle();
    let orch_handle = tokio::spawn(async move { orchestrator.run().await });

    // ---- Linear tracker ----------------------------------------------
    // Tight cadence so the second poll fires well within the test
    // timeout — Linear's 5-minute cap is bypassed by clamping in the
    // tracker, but any cadence at or below the cap is honoured verbatim.
    let endpoint = format!("{}/graphql", server.uri());
    let tracker_config = LinearTrackerConfig {
        endpoint,
        cadence: Duration::from_millis(50),
        scopes: vec![ScopeWatch {
            repo: RepoId::new(TEST_REPO),
            scope: LinearScope::Team {
                key: "ENG".to_string(),
            },
        }],
        token: SecretString::new(TEST_TOKEN),
        rate_limit: Arc::new(NoopRateLimit),
    };
    let tracker = LinearTracker::new(tracker_config);
    let (tracker_shutdown_tx, tracker_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let tracker_handle =
        tokio::spawn(async move { tracker.run(polling_tx, tracker_shutdown_rx).await });

    // ---- Drive: wait for the actor to reach Active -------------------
    // Workspace must exist by the time the actor enters AwaitingReview
    // (workspace creation is gated on the `Queued -> Active` transition
    // commit; the actor only advances past Active after the engine clean
    // exit drives `Active -> AwaitingReview`).
    let reached_awaiting = await_cond(Duration::from_secs(20), || {
        let log = recorded.try_lock();
        match log {
            Ok(entries) => entries
                .iter()
                .any(|ev| ev.next == WorkerState::AwaitingReview),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached_awaiting,
        "actor must reach AwaitingReview within timeout (recorded so far: {:?})",
        recorded.lock().await,
    );

    // Workspace directory MUST exist on disk while the actor is in
    // AwaitingReview / TerminalSuccess (Requirement 4.3 / 4.4). The
    // fake Linear is still serving `started` here, so the actor cannot
    // race ahead to Cleaning before this check completes.
    let expected_workspace = expected_worktree_path(&parent_path, "core", TEST_ISSUE);
    assert!(
        expected_workspace.is_dir(),
        "workspace directory must exist after Active; expected {expected_workspace:?}",
    );

    // Now stage the terminal payload. Resetting the wiremock server
    // drops the `started` mock; mounting `completed` flips every
    // subsequent poll to the terminal payload. The bridge dedupes
    // (repo, issue, state), so the orchestrator observes exactly one
    // additional tracker event for this key.
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completed_payload()))
        .mount(&server)
        .await;

    // ---- Drive: wait for Cleaning ------------------------------------
    // Once the polling loop sees the `completed` payload, the bridge
    // forwards the new state and the orchestrator runs both vetoable
    // transitions back-to-back: AwaitingReview -> TerminalSuccess and
    // TerminalSuccess -> Cleaning. The actor exits after committing the
    // Cleaning transition; workspace removal happens before exit.
    let reached_cleaning = await_cond(Duration::from_secs(30), || {
        let log = recorded.try_lock();
        match log {
            Ok(entries) => entries.iter().any(|ev| ev.next == WorkerState::Cleaning),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached_cleaning,
        "actor must reach Cleaning within timeout (recorded so far: {:?})",
        recorded.lock().await,
    );

    // Workspace removal happens during the `Cleaning` handler, before
    // the actor exits. Wait for the directory to disappear from disk.
    let workspace_for_check = expected_workspace.clone();
    let removed = await_cond(Duration::from_secs(10), || !workspace_for_check.exists()).await;
    assert!(
        removed,
        "workspace directory must be removed after Cleaning; still present at {expected_workspace:?}",
    );

    // ---- Assertions on the published transition log ------------------
    let final_log = recorded.lock().await.clone();

    // Exact documented happy-path sequence, in order.
    let observed_pairs: Vec<(WorkerState, WorkerState)> =
        final_log.iter().map(|ev| (ev.previous, ev.next)).collect();
    let expected_pairs = vec![
        (WorkerState::Discovered, WorkerState::Queued),
        (WorkerState::Queued, WorkerState::Active),
        (WorkerState::Active, WorkerState::AwaitingReview),
        (WorkerState::AwaitingReview, WorkerState::TerminalSuccess),
        (WorkerState::TerminalSuccess, WorkerState::Cleaning),
    ];
    assert_eq!(
        observed_pairs, expected_pairs,
        "transition sequence must match the documented happy-path order",
    );

    // No duplicate transitions for the same (repo, issue) key — the
    // bridge dedup contract holds end-to-end (Requirement 13.2's
    // idempotence note in the design).
    let mut seen = std::collections::HashSet::new();
    for ev in &final_log {
        let key = (
            ev.repo.as_str().to_string(),
            ev.issue.as_str().to_string(),
            ev.previous,
            ev.next,
        );
        assert!(seen.insert(key), "duplicate transition emitted for {ev:?}",);
    }

    // Every transition carries a non-nil correlation id and they all
    // share the same value (one launch per actor, threaded through every
    // committed transition).
    let first_correlation = final_log[0].correlation_id;
    for ev in &final_log {
        assert_ne!(
            ev.correlation_id,
            CorrelationId::from_uuid(uuid::Uuid::nil()),
            "correlation id must be non-nil for {ev:?}",
        );
        assert_eq!(
            ev.correlation_id, first_correlation,
            "correlation id must be stable across the actor's transitions; got mismatch at {ev:?}",
        );
        assert_eq!(ev.repo.as_str(), TEST_REPO);
        assert_eq!(ev.issue.as_str(), TEST_ISSUE);
    }

    // The vetoable flag matches the documented vetoable subset
    // (Requirement 8.2 / 13.2): only Queued->Active,
    // AwaitingReview->TerminalSuccess, and TerminalSuccess->Cleaning are
    // vetoable.
    for ev in &final_log {
        let expected_vetoable = matches!(
            (ev.previous, ev.next),
            (WorkerState::Queued, WorkerState::Active)
                | (WorkerState::AwaitingReview, WorkerState::TerminalSuccess)
                | (WorkerState::TerminalSuccess, WorkerState::Cleaning),
        );
        assert_eq!(
            ev.vetoable, expected_vetoable,
            "vetoable flag wrong for {ev:?}",
        );
    }

    // Snapshot via the read API confirms the actor terminated in
    // Cleaning.
    let snapshot = read_handle.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    assert_eq!(snapshot.issues[0].repo.as_str(), TEST_REPO);
    assert_eq!(snapshot.issues[0].issue.as_str(), TEST_ISSUE);
    assert_eq!(snapshot.issues[0].state, WorkerState::Cleaning);

    // ---- Tear down ----------------------------------------------------
    let _ = tracker_shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), tracker_handle).await;

    // Closing the bridge by dropping its inputs.
    // The polling sender was moved into `tracker.run`; it drops when
    // the tracker task exits. The webhook sender we kept is dropped by
    // going out of scope.
    drop(_webhook_tx);

    let _ = tokio::time::timeout(Duration::from_secs(5), bridge_handle).await;

    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(5), orch_handle).await;
}
