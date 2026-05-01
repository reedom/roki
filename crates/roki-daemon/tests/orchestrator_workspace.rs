//! Integration tests for orchestrator workspace lifecycle wiring (task 3.5).
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

use std::path::{Path, PathBuf};
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
use roki_daemon::workspace::{Workspace, WorkspaceError, WorkspaceManager};

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

/// Records every observed transition. Used to assert that a particular state
/// has been reached without forcing a fixed end-of-test ordering check.
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

/// Pre-cleanup hook that always denies with a recognizable reason. Records
/// invocation count so the test can prove dispatch happened exactly once.
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

/// Reach into the on-disk layout the real `WorkspaceManager` produces and
/// return the expected `<root>/<repo>/<issue>` directory path.
fn expected_workspace_path(root: &Path, repo: &str, issue: &str) -> PathBuf {
    root.join(repo).join(issue)
}

#[tokio::test]
async fn happy_path_workspace_is_created_then_deleted() {
    // Observable-completion criterion 1: with no pre-cleanup hooks registered
    // the workspace is created on activation, transitions through
    // `TerminalSuccess -> Cleaning`, and is removed from disk.
    let workspace_root = tempdir().expect("tempdir");
    let workspace =
        Arc::new(WorkspaceManager::new(workspace_root.path()).expect("workspace manager"));
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

    // Drive into Active so the workspace is created.
    tracker_tx
        .send(sample_issue("repo-a", "ENG-1", TrackerIssueState::Active))
        .await
        .expect("send active");

    // Wait until the actor is at AwaitingReview (workspace must already be on
    // disk before TerminalSuccess is even attempted).
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

    let workspace_path = expected_workspace_path(workspace_root.path(), "repo-a", "ENG-1");
    assert!(
        workspace_path.is_dir(),
        "workspace must exist on disk while issue is Active/AwaitingReview, expected at {workspace_path:?}",
    );

    // Drive to terminal so the actor runs TerminalSuccess -> Cleaning.
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

    // Wait for the actual filesystem removal to complete.
    let workspace_removed =
        await_condition(Duration::from_secs(5), || !workspace_path.exists()).await;
    assert!(
        workspace_removed,
        "workspace must be removed after Cleaning: still present at {workspace_path:?}",
    );

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}

#[tokio::test]
#[tracing_test::traced_test]
async fn deny_hook_retains_workspace_and_logs_veto() {
    // Observable-completion criterion 2: a pre-cleanup hook returning Deny
    // keeps the workspace on disk and emits a veto-decision log entry.
    let workspace_root = tempdir().expect("tempdir");
    let workspace =
        Arc::new(WorkspaceManager::new(workspace_root.path()).expect("workspace manager"));
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

    // Give the hook dispatch and the post-evaluation log emission a moment to
    // resolve; the actor stays in TerminalSuccess after Deny so there is no
    // further transition to wait on.
    let hook_dispatched = await_condition(Duration::from_secs(5), || {
        1 <= invocations.load(Ordering::SeqCst)
    })
    .await;
    assert!(hook_dispatched, "pre-cleanup hook must be dispatched");

    // The actor must NOT have advanced to Cleaning.
    let advanced_to_cleaning = last_state_reached(&recorded, WorkerState::Cleaning).await;
    assert!(
        !advanced_to_cleaning,
        "Deny hook must keep the actor in TerminalSuccess; recorded: {:?}",
        recorded.lock().await,
    );

    // Workspace must still be on disk.
    let workspace_path = expected_workspace_path(workspace_root.path(), "repo-b", "ENG-9");
    assert!(
        workspace_path.is_dir(),
        "workspace must be retained when pre-cleanup hook denies, expected at {workspace_path:?}",
    );

    // Read snapshot must reflect TerminalSuccess.
    let snapshot = read_handle.snapshot();
    assert_eq!(snapshot.issues.len(), 1);
    assert_eq!(snapshot.issues[0].state, WorkerState::TerminalSuccess);

    // The veto decision must be logged with the hook's reason.
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
    // Observable-completion criterion 3: a workspace error during ensure
    // forces TerminalFailure for that (repo, issue), retains no workspace
    // (because creation failed), and refuses any subsequent tracker event for
    // the same key with a structured log.
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

    // Engine must NOT have launched — workspace failed before launch.
    assert_eq!(
        launches.load(Ordering::SeqCst),
        0,
        "engine must not launch when workspace ensure fails",
    );

    // Read projection should report TerminalFailure for the key.
    let one = read_handle
        .issue(&RepoId::new("repo-c"), &IssueId::new("ENG-77"))
        .expect("issue must be tracked");
    assert_eq!(one.state, WorkerState::TerminalFailure);

    // Subsequent tracker events for the same (repo, issue) must be refused.
    // Send another active event; the actor must NOT relaunch and the key must
    // stay TerminalFailure.
    let ensures_before = ensure_calls.load(Ordering::SeqCst);
    tracker_tx
        .send(sample_issue("repo-c", "ENG-77", TrackerIssueState::Active))
        .await
        .expect("send active again");

    // Give the orchestrator a moment to process and reject.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ensure must not be called a second time.
    let ensures_after = ensure_calls.load(Ordering::SeqCst);
    assert_eq!(
        ensures_before, ensures_after,
        "poisoned (repo, issue) must not trigger another workspace ensure",
    );

    // State remains TerminalFailure.
    let still_failed = read_handle
        .issue(&RepoId::new("repo-c"), &IssueId::new("ENG-77"))
        .expect("issue must remain tracked");
    assert_eq!(still_failed.state, WorkerState::TerminalFailure);

    // Logs must mention the workspace error and the refusal of the second
    // tracker event for the poisoned key.
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
