//! Integration tests for orchestrator workspace lifecycle wiring (task 3.5,
//! refactored under task 6.1 to use the worktree workspace model).
//!
//! These tests pin the three observable-completion criteria of task 3.5:
//!
//! 1. Happy path with no pre-cleanup hooks — workspace is created on
//!    activation, transitions through `TerminalSuccess -> Cleaning`, and is
//!    deleted on disk.
//! 2. Pre-cleanup `Deny` — workspace is retained, the veto decision is logged.
//! 3. Workspace `ensure` failure — the issue lands in `TerminalFailure` with
//!    the workspace absent, and subsequent tracker events for the same
//!    `(repo, issue)` are refused (poisoned key) until operator intervenes.

mod common;

use std::path::PathBuf;
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
use roki_daemon::orchestrator::hooks::{HookRegistry, PreCleanupContext, PreCleanupHook};
use roki_daemon::orchestrator::read::OrchestratorRead;
use roki_daemon::orchestrator::state::{
    IssueId, RepoId, TransitionEvent, VetoDecision, WorkerState,
};
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use roki_daemon::workspace::{Workspace, WorkspaceError};

use crate::common::{build_workspace_manager, expected_worktree_path};

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
        let _ = events.send(SupervisedEvent::Exited(self.outcome)).await;
        Ok(self.outcome)
    }
}

/// Records every observed transition.
struct RecordingObserver {
    log: Arc<Mutex<Vec<(WorkerState, WorkerState)>>>,
}

#[async_trait]
impl TransitionSubscriber for RecordingObserver {
    fn id(&self) -> &str {
        "recorder"
    }

    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        self.log.lock().await.push((event.previous, event.next));
        Ok(())
    }
}

fn sample_issue(repo: &str, issue: &str, state: TrackerIssueState) -> NormalizedIssue {
    NormalizedIssue {
        repo: RepoId::new(repo),
        issue: IssueId::new(issue),
        title: "test".to_string(),
        description: "test".to_string(),
        state,
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

async fn last_state_reached(
    log: &Arc<Mutex<Vec<(WorkerState, WorkerState)>>>,
    target: WorkerState,
) -> bool {
    let g = log.lock().await;
    g.iter().any(|(_, next)| *next == target)
}

/// Pre-cleanup hook that always denies with a recognizable reason.
struct DenyHook {
    reason: &'static str,
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl PreCleanupHook for DenyHook {
    async fn pre_cleanup(&self, _ctx: &PreCleanupContext) -> VetoDecision {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        VetoDecision::deny(self.reason)
    }
}

/// Workspace stub whose `ensure` always fails. Used to exercise the workspace
/// creation-error branch of the orchestrator without depending on filesystem
/// quirks (read-only volumes, etc.) that vary across CI environments.
struct FailingWorkspace {
    ensure_calls: Arc<AtomicUsize>,
    remove_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Workspace for FailingWorkspace {
    async fn ensure(&self, _repo: &RepoId, _issue: &IssueId) -> Result<PathBuf, WorkspaceError> {
        self.ensure_calls.fetch_add(1, Ordering::SeqCst);
        Err(WorkspaceError::InvalidIdentifier {
            reason: "synthetic ensure failure for task 3.5 test".to_string(),
        })
    }

    async fn remove(&self, _repo: &RepoId, _issue: &IssueId) -> Result<(), WorkspaceError> {
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn list_existing(&self) -> Result<Vec<(RepoId, IssueId, PathBuf)>, WorkspaceError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn happy_path_workspace_is_created_then_deleted() {
    let parent = tempdir().expect("tempdir");
    let parent_path = parent.path().to_path_buf();
    let (workspace, _parent_keep, mock_wt, _mock_ghq) =
        build_workspace_manager(parent, &[("repo-a", "owner/repo-a", "repo-a")]);
    let workspace = Arc::new(workspace);

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
        Arc::clone(&workspace) as Arc<_>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    );

    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(sample_issue("repo-a", "ENG-1", TrackerIssueState::Active))
        .await
        .expect("send active");

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
    assert!(reached_review, "actor must reach AwaitingReview");

    let workspace_path = expected_worktree_path(&parent_path, "repo-a", "ENG-1");
    assert!(
        workspace_path.is_dir(),
        "workspace must exist on disk while issue is Active/AwaitingReview, expected at {workspace_path:?}",
    );

    tracker_tx
        .send(sample_issue("repo-a", "ENG-1", TrackerIssueState::Terminal))
        .await
        .expect("send terminal");

    let reached_cleaning = await_condition(Duration::from_secs(5), || {
        let log = recorded.try_lock();
        match log {
            Ok(g) => g.iter().any(|(_, next)| *next == WorkerState::Cleaning),
            Err(_) => false,
        }
    })
    .await;
    assert!(reached_cleaning, "actor must reach Cleaning");

    let workspace_removed =
        await_condition(Duration::from_secs(5), || !workspace_path.exists()).await;
    assert!(
        workspace_removed,
        "workspace must be removed after Cleaning: still present at {workspace_path:?}",
    );

    // wt.remove must have been invoked exactly once with the worktree path.
    let removes = mock_wt.remove_calls.lock().unwrap().clone();
    assert_eq!(removes.len(), 1);
    assert_eq!(removes[0], workspace_path);

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

#[tokio::test]
#[tracing_test::traced_test]
async fn deny_hook_retains_workspace_and_logs_veto() {
    let parent = tempdir().expect("tempdir");
    let parent_path = parent.path().to_path_buf();
    let (workspace, _parent_keep, mock_wt, _mock_ghq) =
        build_workspace_manager(parent, &[("repo-b", "owner/repo-b", "repo-b")]);
    let workspace = Arc::new(workspace);
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let invocations = Arc::new(AtomicUsize::new(0));
    hook_registry.register_pre_cleanup_hook(Arc::new(DenyHook {
        reason: "distill-postmerge: pending writeback",
        invocations: Arc::clone(&invocations),
    }));
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
        Arc::clone(&workspace) as Arc<_>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    );
    let read_handle = orchestrator.read_handle();

    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(sample_issue("repo-b", "ENG-9", TrackerIssueState::Active))
        .await
        .expect("send active");

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
    assert!(reached_review, "actor must reach AwaitingReview");

    tracker_tx
        .send(sample_issue("repo-b", "ENG-9", TrackerIssueState::Terminal))
        .await
        .expect("send terminal");

    let reached_terminal_success = await_condition(Duration::from_secs(5), || {
        let log = recorded.try_lock();
        match log {
            Ok(g) => g
                .iter()
                .any(|(_, next)| *next == WorkerState::TerminalSuccess),
            Err(_) => false,
        }
    })
    .await;
    assert!(reached_terminal_success, "actor must reach TerminalSuccess");

    let hook_dispatched = await_condition(Duration::from_secs(5), || {
        1 <= invocations.load(Ordering::SeqCst)
    })
    .await;
    assert!(hook_dispatched, "pre-cleanup hook must be dispatched");

    let advanced_to_cleaning = last_state_reached(&recorded, WorkerState::Cleaning).await;
    assert!(
        !advanced_to_cleaning,
        "Deny hook must keep the actor in TerminalSuccess; recorded: {:?}",
        recorded.lock().await,
    );

    let workspace_path = expected_worktree_path(&parent_path, "repo-b", "ENG-9");
    assert!(
        workspace_path.is_dir(),
        "workspace must be retained when pre-cleanup hook denies, expected at {workspace_path:?}",
    );

    // wt.remove must NOT have been invoked.
    assert!(
        mock_wt.remove_calls.lock().unwrap().is_empty(),
        "Deny hook must skip wt.remove",
    );

    let snapshot = read_handle.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    assert_eq!(snapshot.issues[0].state, WorkerState::TerminalSuccess);

    assert!(
        logs_contain("pre-cleanup hook denied"),
        "deny veto decision must be logged",
    );
    assert!(
        logs_contain("distill-postmerge: pending writeback"),
        "veto reason must appear in logs",
    );

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

#[tokio::test]
#[tracing_test::traced_test]
async fn workspace_ensure_error_lands_in_terminal_failure_and_poisons_key() {
    let ensure_calls = Arc::new(AtomicUsize::new(0));
    let remove_calls = Arc::new(AtomicUsize::new(0));
    let workspace = Arc::new(FailingWorkspace {
        ensure_calls: Arc::clone(&ensure_calls),
        remove_calls: Arc::clone(&remove_calls),
    });
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        log: Arc::clone(&recorded),
    }));

    let launches = Arc::new(AtomicUsize::new(0));
    let engine = Arc::new(StubEngine {
        outcome: WorkerOutcome::CleanExit,
        launches: Arc::clone(&launches),
    });
    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        workspace as Arc<_>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
    );
    let read_handle = orchestrator.read_handle();

    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    tracker_tx
        .send(sample_issue("repo-c", "ENG-77", TrackerIssueState::Active))
        .await
        .expect("send active");

    let reached_failure = await_condition(Duration::from_secs(5), || {
        let log = recorded.try_lock();
        match log {
            Ok(g) => g
                .iter()
                .any(|(_, next)| *next == WorkerState::TerminalFailure),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached_failure,
        "workspace ensure error must drive the actor to TerminalFailure; recorded: {:?}",
        recorded.lock().await,
    );

    assert_eq!(
        launches.load(Ordering::SeqCst),
        0,
        "engine must not launch when workspace ensure fails",
    );

    let one = read_handle
        .issue(&RepoId::new("repo-c"), &IssueId::new("ENG-77"))
        .expect("issue must be tracked");
    assert_eq!(one.state, WorkerState::TerminalFailure);

    let ensures_before = ensure_calls.load(Ordering::SeqCst);
    tracker_tx
        .send(sample_issue("repo-c", "ENG-77", TrackerIssueState::Active))
        .await
        .expect("send active again");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let ensures_after = ensure_calls.load(Ordering::SeqCst);
    assert_eq!(
        ensures_before, ensures_after,
        "poisoned (repo, issue) must not trigger another workspace ensure",
    );

    let still_failed = read_handle
        .issue(&RepoId::new("repo-c"), &IssueId::new("ENG-77"))
        .expect("issue must remain tracked");
    assert_eq!(still_failed.state, WorkerState::TerminalFailure);

    assert!(
        logs_contain("workspace ensure failed"),
        "workspace ensure failure must be logged",
    );
    assert!(
        logs_contain("poisoned"),
        "subsequent tracker events for poisoned (repo, issue) must be logged as refused",
    );

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}
