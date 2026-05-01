//! Orchestrator session-tempdir lifecycle integration tests (task 7.1d).
//!
//! Pinned by the task brief:
//!
//! 1. `Queued -> Active` materialises the per-issue session tempdir on disk.
//! 2. `Cleaning` removes the session tempdir AND walks the worktree
//!    registry to call `wt.remove` per registered worktree.
//! 3. `TerminalFailure` retains BOTH the session tempdir AND every
//!    registered worktree (design decision #6).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use roki_daemon::engine::policy::{BackoffPolicy, EnginePolicy, WorkerOutcome};
use roki_daemon::engine::{SupervisedEvent, WorkerContext};
use roki_daemon::orchestrator::core::{EngineLauncher, LaunchError, Orchestrator};
use roki_daemon::orchestrator::events::{EventBus, SubscriberError, TransitionSubscriber};
use roki_daemon::orchestrator::hooks::HookRegistry;
use roki_daemon::orchestrator::state::{IssueId, RepoId, TransitionEvent, WorkerState};
use roki_daemon::session::SessionManager;
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tools::WtTool;
use roki_daemon::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use roki_daemon::worktrees::{BranchName, WorktreeRegistry};

use crate::common::MockWt;

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
        let _ = events.send(SupervisedEvent::Exited(self.outcome)).await;
        Ok(self.outcome)
    }
}

struct RecordingObserver {
    log: Arc<Mutex<Vec<(WorkerState, WorkerState)>>>,
}

#[async_trait]
impl TransitionSubscriber for RecordingObserver {
    fn id(&self) -> &str {
        "session-recorder"
    }

    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        self.log.lock().await.push((event.previous, event.next));
        Ok(())
    }
}

fn issue_event(state: TrackerIssueState) -> NormalizedIssue {
    NormalizedIssue {
        repo: RepoId::new("any-repo"),
        issue: IssueId::new("ENG-42"),
        title: String::new(),
        description: String::new(),
        state,
        labels: Vec::new(),
    }
}

async fn await_state(
    log: &Arc<Mutex<Vec<(WorkerState, WorkerState)>>>,
    target: WorkerState,
    timeout: Duration,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        let g = log.lock().await;
        if g.iter().any(|(_, next)| *next == target) {
            return true;
        }
        drop(g);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

#[tokio::test]
async fn happy_path_session_tempdir_is_created_on_active_and_removed_on_cleaning() {
    let parent = tempdir().expect("tempdir");
    let session_root = parent.path().join("sessions");
    let session_manager = Arc::new(SessionManager::with_root(session_root.clone()));
    let registry = WorktreeRegistry::new();
    let mock_wt = Arc::new(MockWt::default());
    let wt: Arc<dyn WtTool> = Arc::clone(&mock_wt) as Arc<dyn WtTool>;

    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();
    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        log: Arc::clone(&recorded),
    }));

    let engine = Arc::new(StubEngine {
        outcome: WorkerOutcome::CleanExit,
        launches: Arc::new(AtomicUsize::new(0)),
    });
    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&session_manager),
        registry.clone(),
        wt,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    );
    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(issue_event(TrackerIssueState::Active))
        .await
        .unwrap();

    let reached_review = await_state(
        &recorded,
        WorkerState::AwaitingReview,
        Duration::from_secs(5),
    )
    .await;
    assert!(reached_review, "actor must reach AwaitingReview");

    // Session tempdir must exist on disk.
    let session_path = session_root.join("ENG-42");
    assert!(
        session_path.is_dir(),
        "session tempdir must exist while issue is Active/AwaitingReview at {session_path:?}",
    );

    tracker_tx
        .send(issue_event(TrackerIssueState::Terminal))
        .await
        .unwrap();

    let reached_cleaning =
        await_state(&recorded, WorkerState::Cleaning, Duration::from_secs(5)).await;
    assert!(reached_cleaning, "actor must reach Cleaning");

    // Session tempdir must be removed.
    let removed = {
        let start = std::time::Instant::now();
        loop {
            if !session_path.exists() {
                break true;
            }
            if Duration::from_secs(2) <= start.elapsed() {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    assert!(removed, "session tempdir must be removed after Cleaning");

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

#[tokio::test]
async fn cleaning_walks_registry_and_removes_every_registered_worktree() {
    // Pre-register two worktrees against the issue, drive through to
    // Cleaning, and assert wt.remove is called once per entry in
    // registration order.
    let parent = tempdir().expect("tempdir");
    let session_root = parent.path().join("sessions");
    let session_manager = Arc::new(SessionManager::with_root(session_root.clone()));
    let registry = WorktreeRegistry::new();

    // Pre-register two worktrees as if the agent had called the tool twice.
    let issue = IssueId::new("ENG-42");
    let path_a = parent.path().join("core.ENG-42");
    let path_b = parent.path().join("infra.ENG-42");
    std::fs::create_dir_all(&path_a).unwrap();
    std::fs::create_dir_all(&path_b).unwrap();
    registry.register(
        issue.clone(),
        RepoId::new("owner/core"),
        BranchName::new("ENG-42"),
        path_a.clone(),
    );
    registry.register(
        issue.clone(),
        RepoId::new("owner/infra"),
        BranchName::new("ENG-42"),
        path_b.clone(),
    );

    let mock_wt = Arc::new(MockWt::default());
    let wt: Arc<dyn WtTool> = Arc::clone(&mock_wt) as Arc<dyn WtTool>;

    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();
    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        log: Arc::clone(&recorded),
    }));

    let engine = Arc::new(StubEngine {
        outcome: WorkerOutcome::CleanExit,
        launches: Arc::new(AtomicUsize::new(0)),
    });
    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&session_manager),
        registry.clone(),
        wt,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    );
    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(issue_event(TrackerIssueState::Active))
        .await
        .unwrap();
    let reached_review = await_state(
        &recorded,
        WorkerState::AwaitingReview,
        Duration::from_secs(5),
    )
    .await;
    assert!(reached_review);

    tracker_tx
        .send(issue_event(TrackerIssueState::Terminal))
        .await
        .unwrap();
    let reached_cleaning =
        await_state(&recorded, WorkerState::Cleaning, Duration::from_secs(5)).await;
    assert!(reached_cleaning);

    // Wait for the cleanup walk to settle.
    let removed = {
        let start = std::time::Instant::now();
        loop {
            let calls = mock_wt.remove_calls.lock().unwrap().clone();
            if calls.len() == 2 {
                break Some(calls);
            }
            if Duration::from_secs(2) <= start.elapsed() {
                break None;
            }
            drop(calls);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let calls = removed.expect("wt.remove must be called twice");
    // Ordering: registration order (owner/core then owner/infra).
    assert_eq!(calls[0], path_a);
    assert_eq!(calls[1], path_b);
    // Registry is drained.
    assert!(registry.list_for_issue(&issue).is_empty());
    // Session tempdir removed.
    let session_path = session_root.join("ENG-42");
    let session_gone = {
        let start = std::time::Instant::now();
        loop {
            if !session_path.exists() {
                break true;
            }
            if Duration::from_secs(2) <= start.elapsed() {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    assert!(
        session_gone,
        "session tempdir must be removed after cleaning walk"
    );

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

fn fast_retry_policy(max_attempts: u32) -> EnginePolicy {
    EnginePolicy {
        backoff: BackoffPolicy {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(20),
            multiplier: 1.0,
        },
        backoff_floor: Duration::from_millis(20),
        max_attempts,
        ..EnginePolicy::default()
    }
}

#[tokio::test]
async fn terminal_failure_retains_session_tempdir_and_worktrees() {
    // Design decision #6: on TerminalFailure, BOTH session tempdir AND every
    // registered worktree are retained. The orchestrator walks neither.
    let parent = tempdir().expect("tempdir");
    let session_root = parent.path().join("sessions");
    let session_manager = Arc::new(SessionManager::with_root(session_root.clone()));
    let registry = WorktreeRegistry::new();

    // Pre-register a worktree as if the agent had opened one before the
    // retry-budget exhaustion fired.
    let issue = IssueId::new("ENG-42");
    let registered_path = parent.path().join("core.ENG-42");
    std::fs::create_dir_all(&registered_path).unwrap();
    registry.register(
        issue.clone(),
        RepoId::new("owner/core"),
        BranchName::new("ENG-42"),
        registered_path.clone(),
    );

    let mock_wt = Arc::new(MockWt::default());
    let wt: Arc<dyn WtTool> = Arc::clone(&mock_wt) as Arc<dyn WtTool>;

    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();
    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        log: Arc::clone(&recorded),
    }));

    let engine = Arc::new(StubEngine {
        outcome: WorkerOutcome::NonCleanExit { code: 1 },
        launches: Arc::new(AtomicUsize::new(0)),
    });
    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&session_manager),
        registry.clone(),
        wt,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    )
    .with_engine_policy(fast_retry_policy(1));

    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(issue_event(TrackerIssueState::Active))
        .await
        .unwrap();

    let reached_failure = await_state(
        &recorded,
        WorkerState::TerminalFailure,
        Duration::from_secs(5),
    )
    .await;
    assert!(reached_failure, "actor must reach TerminalFailure");

    // Session tempdir retained.
    let session_path = session_root.join("ENG-42");
    assert!(
        session_path.is_dir(),
        "session tempdir must be retained after TerminalFailure",
    );
    // Worktree retained: wt.remove was NEVER called.
    assert!(
        mock_wt.remove_calls.lock().unwrap().is_empty(),
        "wt.remove must not be called on TerminalFailure",
    );
    // Registered path still on disk.
    assert!(
        registered_path.is_dir(),
        "registered worktree must be retained on TerminalFailure",
    );
    // Registry entry untouched.
    let entries = registry.list_for_issue(&issue);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, registered_path);

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}
