//! Cross-repo agent-driven worktree e2e (task 7.1d).
//!
//! Pinned by the task brief: "a new cross-repo e2e test where one worker
//! opens worktrees in two configured repos." The test wires the
//! `roki_open_worktree` agent tool against two configured repos, has the
//! agent open a worktree in each, drives the worker to `Cleaning`, and
//! asserts:
//!
//! 1. Both worktrees are registered under the same issue (insertion order).
//! 2. `wt.remove` is called once per registered worktree on `Cleaning` in
//!    registration order.
//! 3. The session tempdir is removed alongside the worktrees.

mod common;

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
use roki_daemon::orchestrator::state::{IssueId, TransitionEvent, WorkerState};
use roki_daemon::session::SessionManager;
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tools::{GhqTool, OpenWorktreeTool, Tool, WtTool};
use roki_daemon::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use roki_daemon::worktrees::WorktreeRegistry;

use crate::common::{MockGhq, MockWt};

/// Engine stub that, on launch, drives the agent-tool for two configured
/// repos under the worker's issue. This stands in for a real `claude`
/// subprocess that would call the tool from inside the agent's reasoning
/// loop. After the cross-repo opens, the stub emits a CleanExit terminal.
struct AgentStubEngine {
    tool: Arc<OpenWorktreeTool>,
    launches: Arc<AtomicUsize>,
}

#[async_trait]
impl EngineLauncher for AgentStubEngine {
    async fn launch(
        &self,
        _ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        // Open core, then infra — distinct repos under the same issue.
        let _ = self
            .tool
            .call(serde_json::json!({"repo": "owner/core"}))
            .await;
        let _ = self
            .tool
            .call(serde_json::json!({"repo": "owner/infra"}))
            .await;
        let _ = events
            .send(SupervisedEvent::Exited(WorkerOutcome::CleanExit))
            .await;
        Ok(WorkerOutcome::CleanExit)
    }
}

struct RecordingObserver {
    log: Arc<Mutex<Vec<(WorkerState, WorkerState)>>>,
}

#[async_trait]
impl TransitionSubscriber for RecordingObserver {
    fn id(&self) -> &str {
        "cross-repo-recorder"
    }

    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        self.log.lock().await.push((event.previous, event.next));
        Ok(())
    }
}

fn issue_event(state: TrackerIssueState) -> NormalizedIssue {
    NormalizedIssue {
        issue: IssueId::new("ENG-7"),
        title: String::new(),
        description: String::new(),
        state,
        labels: Vec::new(),
        assignee_user_id: None,
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
async fn cross_repo_worker_opens_worktrees_in_two_configured_repos() {
    let parent = tempdir().expect("tempdir");
    let session_root = parent.path().join("sessions");
    let session_manager = Arc::new(SessionManager::with_root(session_root.clone()));
    let registry = WorktreeRegistry::new();

    // Two configured repos: `owner/core` and `owner/infra`. Each backs onto
    // a real on-disk dir under the test parent so `wt switch --create`
    // (the mock) can materialise sibling worktree paths.
    let ghq = Arc::new(MockGhq::new(
        parent.path(),
        &[("owner/core", "core"), ("owner/infra", "infra")],
    ));
    let mock_wt = Arc::new(MockWt::default());
    let wt: Arc<dyn WtTool> = Arc::clone(&mock_wt) as Arc<dyn WtTool>;

    let issue = IssueId::new("ENG-7");
    let tool = Arc::new(OpenWorktreeTool::new(
        issue.clone(),
        vec!["owner/core".to_string(), "owner/infra".to_string()],
        Arc::clone(&ghq) as Arc<dyn GhqTool>,
        Arc::clone(&wt),
        registry.clone(),
    ));

    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();
    let recorded = Arc::new(Mutex::new(Vec::<(WorkerState, WorkerState)>::new()));
    event_bus.register(Arc::new(RecordingObserver {
        log: Arc::clone(&recorded),
    }));

    let launches = Arc::new(AtomicUsize::new(0));
    let engine = Arc::new(AgentStubEngine {
        tool: Arc::clone(&tool),
        launches: Arc::clone(&launches),
    });
    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&session_manager),
        registry.clone(),
        Arc::clone(&wt),
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

    // Both worktrees are registered under the issue, in insertion order.
    let entries = registry.list_for_issue(&issue);
    assert_eq!(
        entries.len(),
        2,
        "both repos must register worktree entries"
    );
    assert_eq!(entries[0].repo.as_str(), "owner/core");
    assert_eq!(entries[1].repo.as_str(), "owner/infra");
    let core_path = entries[0].path.clone();
    let infra_path = entries[1].path.clone();
    assert!(core_path.is_dir());
    assert!(infra_path.is_dir());

    // Drive to terminal so the orchestrator runs the cleanup walk.
    tracker_tx
        .send(issue_event(TrackerIssueState::Terminal))
        .await
        .unwrap();
    let reached_cleaning =
        await_state(&recorded, WorkerState::Cleaning, Duration::from_secs(5)).await;
    assert!(reached_cleaning, "actor must reach Cleaning");

    // wt.remove must have been called exactly once per registered worktree
    // in registration order.
    let remove_calls = {
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
    let calls = remove_calls.expect("wt.remove must be invoked twice");
    assert_eq!(calls[0], core_path);
    assert_eq!(calls[1], infra_path);

    // Session tempdir removed.
    let session_path = session_root.join("ENG-7");
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
        "session tempdir must be removed after Cleaning"
    );

    // Registry entries drained.
    assert!(registry.list_for_issue(&issue).is_empty());

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}
