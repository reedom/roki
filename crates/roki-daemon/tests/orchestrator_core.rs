//! Integration tests for `orchestrator::core` (task 3.2).
//!
//! These tests exercise the orchestrator runtime end-to-end with stubs
//! standing in for the tracker and engine adapters. Two observable-completion
//! criteria are pinned by the task:
//!
//! 1. Drive an issue from `Discovered` through to `Cleaning` and assert the
//!    published transition sequence (including `TerminalSuccess -> Cleaning`)
//!    and subscriber dispatch order.
//! 2. Read `OrchestratorRead::snapshot` mid-run and assert the projection
//!    matches the actual state.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use roki_daemon::engine::policy::{BackoffPolicy, EnginePolicy, StallReason, WorkerOutcome};
use roki_daemon::engine::{SupervisedEvent, WorkerContext};
use roki_daemon::orchestrator::core::{EngineLauncher, LaunchError, Orchestrator};
use roki_daemon::orchestrator::events::{EventBus, SubscriberError, TransitionSubscriber};
use roki_daemon::orchestrator::hooks::HookRegistry;
use roki_daemon::orchestrator::read::OrchestratorRead;
use roki_daemon::orchestrator::state::{IssueId, RepoId, TransitionEvent, WorkerState};
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use roki_daemon::workspace::Workspace;

mod common;
use crate::common::build_workspace_manager;

/// Engine stub that emits a fixed terminal outcome for every launch.
struct StubEngine {
    outcome: WorkerOutcome,
    launches: Arc<AtomicUsize>,
}

#[async_trait]
impl EngineLauncher for StubEngine {
    async fn launch(
        &self,
        _ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        // Mirror the real adapter's invariant: emit exactly one terminal
        // Exited event per launch.
        let _ = events.send(SupervisedEvent::Exited(self.outcome)).await;
        Ok(self.outcome)
    }
}

/// Records every observed transition event in the order received. Used to
/// assert subscriber dispatch order.
struct RecordingObserver {
    id: &'static str,
    log: Arc<Mutex<Vec<(WorkerState, WorkerState)>>>,
}

#[async_trait]
impl TransitionSubscriber for RecordingObserver {
    fn id(&self) -> &str {
        self.id
    }

    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        self.log.lock().await.push((event.previous, event.next));
        Ok(())
    }
}

fn sample_issue(repo: &str, issue: &str) -> NormalizedIssue {
    NormalizedIssue {
        repo: RepoId::new(repo),
        issue: IssueId::new(issue),
        title: "test".to_string(),
        description: "test".to_string(),
        state: TrackerIssueState::Active,
        labels: Vec::new(),
        team_or_scope: "ENG".to_string(),
    }
}

async fn await_condition<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    while !cond() {
        if timeout <= start.elapsed() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

#[tokio::test]
async fn orchestrator_drives_issue_from_discovered_to_cleaning() {
    // The observable-completion criterion from tasks.md task 3.2: drive an
    // issue from `Discovered` to `Cleaning` and assert the published
    // transition sequence (including `TerminalSuccess -> Cleaning`) plus
    // subscriber dispatch order.
    let parent = tempdir().expect("tempdir");
    let (manager, _parent_keep, _wt, _ghq) =
        build_workspace_manager(parent, &[("repo-a", "owner/repo-a", "repo-a")]);
    let workspace = Arc::new(manager);
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        id: "recorder",
        log: Arc::clone(&recorded),
    }));

    let launches = Arc::new(AtomicUsize::new(0));
    let engine = Arc::new(StubEngine {
        outcome: WorkerOutcome::CleanExit,
        launches: Arc::clone(&launches),
    });

    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&workspace) as Arc<_>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    );
    let read_handle = orchestrator.read_handle();

    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    // Drive: tracker emits one active issue.
    tracker_tx
        .send(sample_issue("repo-a", "ENG-1"))
        .await
        .expect("tracker send");

    // Mark issue as terminal (resolved) so the actor can move from
    // AwaitingReview through TerminalSuccess to Cleaning.
    tracker_tx
        .send(NormalizedIssue {
            state: TrackerIssueState::Terminal,
            ..sample_issue("repo-a", "ENG-1")
        })
        .await
        .expect("tracker terminal send");

    // Wait for the actor to reach Cleaning. Cleaning is the documented
    // terminal-end (no outgoing transitions).
    let reached_cleaning = await_condition(Duration::from_secs(5), || {
        let captured = recorded.try_lock();
        match captured {
            Ok(log) => log
                .last()
                .map(|(_, next)| *next == WorkerState::Cleaning)
                .unwrap_or(false),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached_cleaning,
        "actor must reach Cleaning; recorded so far: {:?}",
        recorded.lock().await
    );

    let final_log = recorded.lock().await.clone();

    // The expected sequence of transitions, in order, all via the same
    // single-subscriber observer (so dispatch order equals publish order):
    //   Discovered -> Queued
    //   Queued -> Active
    //   Active -> AwaitingReview
    //   AwaitingReview -> TerminalSuccess
    //   TerminalSuccess -> Cleaning
    let expected = vec![
        (WorkerState::Discovered, WorkerState::Queued),
        (WorkerState::Queued, WorkerState::Active),
        (WorkerState::Active, WorkerState::AwaitingReview),
        (WorkerState::AwaitingReview, WorkerState::TerminalSuccess),
        (WorkerState::TerminalSuccess, WorkerState::Cleaning),
    ];
    assert_eq!(
        final_log, expected,
        "published transition sequence must match documented happy path",
    );

    assert_eq!(
        launches.load(Ordering::SeqCst),
        1,
        "engine launched exactly once for the active worker",
    );

    // The OrchestratorRead snapshot taken AFTER the actor reaches Cleaning
    // must reflect the same key projected to Cleaning.
    let snapshot = read_handle.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    assert_eq!(snapshot.issues[0].issue.as_str(), "ENG-1");
    assert_eq!(snapshot.issues[0].state, WorkerState::Cleaning);

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

#[tokio::test]
async fn orchestrator_read_snapshot_matches_actual_state_mid_run() {
    // Pause execution mid-state, call OrchestratorRead::snapshot(), and
    // assert the projection matches the actual state.
    //
    // We hold the actor at `Active` by registering a vetoable subscriber that
    // denies `AwaitingReview -> TerminalSuccess` indefinitely (any further
    // promotion would require operator intervention beyond this test). With
    // the stub engine emitting `CleanExit`, the actor will pass through
    // Discovered -> Queued -> Active -> AwaitingReview and then stay there
    // because Terminal isn't sent yet. We snapshot at AwaitingReview.
    let parent = tempdir().expect("tempdir");
    let (manager, _parent_keep, _wt, _ghq) =
        build_workspace_manager(parent, &[("repo-a", "owner/repo-a", "repo-a")]);
    let workspace = Arc::new(manager);
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        id: "recorder",
        log: Arc::clone(&recorded),
    }));

    let engine = Arc::new(StubEngine {
        outcome: WorkerOutcome::CleanExit,
        launches: Arc::new(AtomicUsize::new(0)),
    });
    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        workspace,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    );
    let read_handle = orchestrator.read_handle();
    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(sample_issue("repo-a", "ENG-7"))
        .await
        .expect("tracker send");

    // Wait until the actor reaches AwaitingReview.
    let reached_review = await_condition(Duration::from_secs(5), || {
        let log = recorded.try_lock();
        match log {
            Ok(g) => g
                .iter()
                .any(|(_, next)| *next == WorkerState::AwaitingReview),
            Err(_) => false,
        }
    })
    .await;
    assert!(reached_review, "actor must reach AwaitingReview mid-run");

    let snapshot = read_handle.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    let projected = &snapshot.issues[0];
    assert_eq!(projected.issue.as_str(), "ENG-7");
    assert_eq!(projected.state, WorkerState::AwaitingReview);

    // Single-issue lookup also exposes the same projection.
    let one = read_handle
        .issue(&IssueId::new("ENG-7"))
        .expect("issue must be tracked");
    assert_eq!(one.state, WorkerState::AwaitingReview);

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

/// Engine stub that drives a scripted sequence of [`WorkerOutcome`] values
/// across successive launches. Used by the retry-budget tests pinned by
/// task 3.7. Each call to `launch` pops the next scripted outcome; once the
/// script is exhausted, the stub returns the documented "stuck" outcome
/// (`NonCleanExit { code: 99 }`) to keep the orchestrator from observing
/// `None` and staying in `Active`.
struct ScriptedEngine {
    script: tokio::sync::Mutex<std::collections::VecDeque<WorkerOutcome>>,
    launches: Arc<AtomicUsize>,
}

#[async_trait]
impl EngineLauncher for ScriptedEngine {
    async fn launch(
        &self,
        _ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError> {
        let outcome = {
            let mut script = self.script.lock().await;
            script
                .pop_front()
                .unwrap_or(WorkerOutcome::NonCleanExit { code: 99 })
        };
        self.launches.fetch_add(1, Ordering::SeqCst);
        let _ = events.send(SupervisedEvent::Exited(outcome)).await;
        Ok(outcome)
    }
}

/// Extract the `(previous, next)` pair sequence from the recorded transition
/// log so assertions read like the documented state-machine table.
async fn pairs(
    recorded: &Arc<Mutex<Vec<(WorkerState, WorkerState)>>>,
) -> Vec<(WorkerState, WorkerState)> {
    recorded.lock().await.clone()
}

/// Sub-second floor + retry budget configurable via `max_attempts`, with a
/// fixed (non-growing) per-policy backoff so retry traces are easy to verify
/// deterministically. The absolute `BACKOFF_FLOOR` constant defaults at 10s in
/// production; this test policy overrides it to 50ms via the new
/// `backoff_floor` field so the entire retry trace completes in well under
/// one second.
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

#[tokio::test]
async fn non_clean_exit_drives_active_backoff_loop_until_budget_exhausted() {
    // Task 3.7 observable completion: with `max_attempts = 3` and a stub
    // returning `NonCleanExit` on every launch, the worker actor must drive
    // exactly the documented retry trace:
    //
    //   Discovered → Queued
    //   Queued → Active     (vetoable: yes)
    //   Active → Backoff    (attempt 1 failed, budget remains)
    //   Backoff → Active    (relaunch)
    //   Active → Backoff    (attempt 2 failed, budget remains)
    //   Backoff → Active    (relaunch)
    //   Active → TerminalFailure (attempt 3 failed, budget exhausted)
    //
    // The engine launcher is invoked exactly `max_attempts = 3` times.
    let parent = tempdir().expect("tempdir");
    let (manager, _parent_keep, _wt, _ghq) =
        build_workspace_manager(parent, &[("repo-a", "owner/repo-a", "repo-a")]);
    let workspace = Arc::new(manager);
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        id: "retry-recorder",
        log: Arc::clone(&recorded),
    }));

    let launches = Arc::new(AtomicUsize::new(0));
    let script: std::collections::VecDeque<WorkerOutcome> = vec![
        WorkerOutcome::NonCleanExit { code: 1 },
        WorkerOutcome::NonCleanExit { code: 2 },
        WorkerOutcome::NonCleanExit { code: 3 },
    ]
    .into();
    let engine = Arc::new(ScriptedEngine {
        script: tokio::sync::Mutex::new(script),
        launches: Arc::clone(&launches),
    });

    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&workspace) as Arc<dyn Workspace>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    )
    .with_engine_policy(fast_retry_policy(3));

    let read_handle = orchestrator.read_handle();
    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(sample_issue("repo-a", "ENG-1"))
        .await
        .expect("tracker send");

    let reached_failure = await_condition(Duration::from_secs(5), || {
        let log = recorded.try_lock();
        match log {
            Ok(entries) => entries
                .iter()
                .any(|(_, next)| *next == WorkerState::TerminalFailure),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached_failure,
        "actor must reach TerminalFailure within timeout (recorded so far: {:?})",
        recorded.lock().await,
    );

    let observed = pairs(&recorded).await;
    let expected = vec![
        (WorkerState::Discovered, WorkerState::Queued),
        (WorkerState::Queued, WorkerState::Active),
        (WorkerState::Active, WorkerState::Backoff),
        (WorkerState::Backoff, WorkerState::Active),
        (WorkerState::Active, WorkerState::Backoff),
        (WorkerState::Backoff, WorkerState::Active),
        (WorkerState::Active, WorkerState::TerminalFailure),
    ];
    assert_eq!(
        observed, expected,
        "retry trace must match the documented Active → Backoff → Active → … → TerminalFailure sequence",
    );

    assert_eq!(
        launches.load(Ordering::SeqCst),
        3,
        "engine must be launched exactly max_attempts (=3) times",
    );

    // TODO(7.1d): re-assert "workspace path retained across the Backoff
    // loop" once `SessionManager` materialises a real session tempdir on
    // `Queued -> Active`. The post-7.1b NoOp shim does not create any
    // directory on disk, so the previous `expected_workspace.is_dir()`
    // probe is dropped here. The retry-budget state-trace assertion above
    // covers the in-memory invariant.

    let snapshot = read_handle.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    assert_eq!(snapshot.issues[0].state, WorkerState::TerminalFailure);

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

#[tokio::test]
async fn stalled_outcome_drives_active_to_terminal_failure_without_backoff() {
    // Task 3.7: `Stalled` is an agent-authored failure — re-running with the
    // same prompt and budget repeats the same outcome — so the worker actor
    // must skip the Backoff loop and route directly to `TerminalFailure`.
    let parent = tempdir().expect("tempdir");
    let (manager, _parent_keep, _wt, _ghq) =
        build_workspace_manager(parent, &[("repo-b", "owner/repo-b", "repo-b")]);
    let workspace = Arc::new(manager);
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        id: "stalled-recorder",
        log: Arc::clone(&recorded),
    }));

    let launches = Arc::new(AtomicUsize::new(0));
    let script: std::collections::VecDeque<WorkerOutcome> = vec![WorkerOutcome::Stalled {
        reason: StallReason::EventInactivity,
    }]
    .into();
    let engine = Arc::new(ScriptedEngine {
        script: tokio::sync::Mutex::new(script),
        launches: Arc::clone(&launches),
    });

    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&workspace) as Arc<dyn Workspace>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    )
    .with_engine_policy(fast_retry_policy(3));

    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(sample_issue("repo-b", "ENG-9"))
        .await
        .expect("tracker send");

    let reached_failure = await_condition(Duration::from_secs(5), || {
        let log = recorded.try_lock();
        match log {
            Ok(entries) => entries
                .iter()
                .any(|(_, next)| *next == WorkerState::TerminalFailure),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached_failure,
        "actor must reach TerminalFailure within timeout (recorded so far: {:?})",
        recorded.lock().await,
    );

    let observed = pairs(&recorded).await;
    let expected = vec![
        (WorkerState::Discovered, WorkerState::Queued),
        (WorkerState::Queued, WorkerState::Active),
        (WorkerState::Active, WorkerState::TerminalFailure),
    ];
    assert_eq!(
        observed, expected,
        "Stalled must route Active → TerminalFailure directly, with no Backoff cycle",
    );

    assert_eq!(
        launches.load(Ordering::SeqCst),
        1,
        "engine must launch exactly once for Stalled (no retry)",
    );

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

#[tokio::test]
async fn turn_budget_exhausted_drives_active_to_terminal_failure_without_backoff() {
    // Task 3.7: `TurnBudgetExhausted` is also an agent-authored failure;
    // retrying with the same budget would reproduce the same outcome.
    let parent = tempdir().expect("tempdir");
    let (manager, _parent_keep, _wt, _ghq) =
        build_workspace_manager(parent, &[("repo-c", "owner/repo-c", "repo-c")]);
    let workspace = Arc::new(manager);
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        id: "turnbudget-recorder",
        log: Arc::clone(&recorded),
    }));

    let launches = Arc::new(AtomicUsize::new(0));
    let script: std::collections::VecDeque<WorkerOutcome> =
        vec![WorkerOutcome::TurnBudgetExhausted].into();
    let engine = Arc::new(ScriptedEngine {
        script: tokio::sync::Mutex::new(script),
        launches: Arc::clone(&launches),
    });

    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&workspace) as Arc<dyn Workspace>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    )
    .with_engine_policy(fast_retry_policy(3));

    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(sample_issue("repo-c", "ENG-7"))
        .await
        .expect("tracker send");

    let reached_failure = await_condition(Duration::from_secs(5), || {
        let log = recorded.try_lock();
        match log {
            Ok(entries) => entries
                .iter()
                .any(|(_, next)| *next == WorkerState::TerminalFailure),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached_failure,
        "actor must reach TerminalFailure within timeout (recorded so far: {:?})",
        recorded.lock().await,
    );

    let observed = pairs(&recorded).await;
    let expected = vec![
        (WorkerState::Discovered, WorkerState::Queued),
        (WorkerState::Queued, WorkerState::Active),
        (WorkerState::Active, WorkerState::TerminalFailure),
    ];
    assert_eq!(
        observed, expected,
        "TurnBudgetExhausted must route Active → TerminalFailure directly, with no Backoff cycle",
    );

    assert_eq!(
        launches.load(Ordering::SeqCst),
        1,
        "engine must launch exactly once for TurnBudgetExhausted (no retry)",
    );

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

#[tokio::test]
#[ignore = "TODO(7.1d): post-7.1b the workspace lifecycle on Queued -> Active \
            is a NoOp shim that does not materialise a directory on disk. \
            Re-enable once SessionManager creates a real session tempdir; \
            the test asserts the path exists during every Backoff window."]
async fn workspace_path_retained_across_backoff_loop() {
    // Task 3.7 explicit observable completion: confirm the workspace path
    // exists on disk during every Backoff window of the retry loop. We pin
    // a subscriber that records existence at each `Active → Backoff`
    // transition and assert the workspace was present every time.
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    let parent = tempdir().expect("tempdir");
    let parent_path = parent.path().to_path_buf();
    let (manager, _parent_keep, _wt, _ghq) =
        build_workspace_manager(parent, &[("repo-d", "owner/repo-d", "repo-d")]);
    let workspace = Arc::new(manager);
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        id: "workspace-recorder",
        log: Arc::clone(&recorded),
    }));

    /// Subscriber that asserts the workspace exists at every
    /// `Active → Backoff` transition.
    struct WorkspaceProbe {
        observations: Arc<StdMutex<Vec<bool>>>,
        expected_dir: PathBuf,
    }

    #[async_trait]
    impl TransitionSubscriber for WorkspaceProbe {
        fn id(&self) -> &str {
            "workspace-probe"
        }
        async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
            if event.previous == WorkerState::Active && event.next == WorkerState::Backoff {
                let exists = self.expected_dir.is_dir();
                self.observations.lock().expect("probe lock").push(exists);
            }
            Ok(())
        }
    }

    let observations = Arc::new(StdMutex::new(Vec::<bool>::new()));
    let expected_dir = crate::common::expected_worktree_path(&parent_path, "repo-d", "ENG-2");
    event_bus.register(Arc::new(WorkspaceProbe {
        observations: Arc::clone(&observations),
        expected_dir: expected_dir.clone(),
    }));

    let launches = Arc::new(AtomicUsize::new(0));
    let script: std::collections::VecDeque<WorkerOutcome> = vec![
        WorkerOutcome::NonCleanExit { code: 1 },
        WorkerOutcome::NonCleanExit { code: 2 },
        WorkerOutcome::NonCleanExit { code: 3 },
    ]
    .into();
    let engine = Arc::new(ScriptedEngine {
        script: tokio::sync::Mutex::new(script),
        launches: Arc::clone(&launches),
    });

    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&workspace) as Arc<dyn Workspace>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    )
    .with_engine_policy(fast_retry_policy(3));

    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(sample_issue("repo-d", "ENG-2"))
        .await
        .expect("tracker send");

    let reached_failure = await_condition(Duration::from_secs(5), || {
        let log = recorded.try_lock();
        match log {
            Ok(entries) => entries
                .iter()
                .any(|(_, next)| *next == WorkerState::TerminalFailure),
            Err(_) => false,
        }
    })
    .await;
    assert!(reached_failure, "actor must reach TerminalFailure");

    let snapshot = observations.lock().expect("observations lock").clone();
    assert_eq!(
        snapshot.len(),
        2,
        "expected exactly 2 Active → Backoff observations for max_attempts=3",
    );
    assert!(
        snapshot.iter().all(|present| *present),
        "workspace must be present during every Backoff window: observations={snapshot:?}",
    );

    // After TerminalFailure the workspace is also retained for inspection.
    assert!(
        expected_dir.is_dir(),
        "workspace must remain on disk after TerminalFailure",
    );

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}
