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

use roki_daemon::engine::policy::WorkerOutcome;
use roki_daemon::engine::{SupervisedEvent, WorkerContext};
use roki_daemon::orchestrator::core::{EngineLauncher, LaunchError, Orchestrator};
use roki_daemon::orchestrator::events::{EventBus, SubscriberError, TransitionSubscriber};
use roki_daemon::orchestrator::hooks::HookRegistry;
use roki_daemon::orchestrator::read::OrchestratorRead;
use roki_daemon::orchestrator::state::{IssueId, RepoId, TransitionEvent, WorkerState};
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use roki_daemon::workspace::WorkspaceManager;

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
    let workspace_root = tempdir().expect("tempdir");
    let workspace =
        Arc::new(WorkspaceManager::new(workspace_root.path()).expect("workspace manager"));
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
    assert_eq!(snapshot.issues[0].repo.as_str(), "repo-a");
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
    let workspace_root = tempdir().expect("tempdir");
    let workspace =
        Arc::new(WorkspaceManager::new(workspace_root.path()).expect("workspace manager"));
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
    assert_eq!(projected.repo.as_str(), "repo-a");
    assert_eq!(projected.issue.as_str(), "ENG-7");
    assert_eq!(projected.state, WorkerState::AwaitingReview);

    // Single-issue lookup also exposes the same projection.
    let one = read_handle
        .issue(&RepoId::new("repo-a"), &IssueId::new("ENG-7"))
        .expect("issue must be tracked");
    assert_eq!(one.state, WorkerState::AwaitingReview);

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}
