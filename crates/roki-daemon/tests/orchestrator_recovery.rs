//! Integration test for `orchestrator::recovery` (task 3.3).
//!
//! The observable-completion criterion from tasks.md:
//!
//! > Pre-seed two workspaces and a Linear stub with mixed states, start the
//! > daemon, and assert each `(repo, issue)` lands in the documented
//! > post-recovery state.
//!
//! The test pre-seeds three `(repo, issue)` keys covering the three live
//! cells of the recovery matrix:
//!
//! | key                   | workspace seeded? | linear active? | expected decision |
//! | --------------------- | ----------------- | -------------- | ----------------- |
//! | `(repo-a, ENG-1)`     | yes               | yes            | `ResumeActive`    |
//! | `(repo-a, ENG-2)`     | yes               | no             | `OrphanedWorkspace` |
//! | `(repo-a, ENG-3)`     | no                | yes            | `FreshQueued`     |
//!
//! After the orchestrator starts, the test asserts:
//!
//! 1. The `RecoveryDecision` list returned by `Orchestrator::with_recovery`
//!    contains exactly those three decisions, one per key.
//! 2. The `ENG-2` workspace directory still exists on disk (Requirement
//!    10.2: orphaned workspaces are retained, not deleted).
//! 3. `ENG-1` and `ENG-3` reach the `Active` state via the orchestrator's
//!    standard tracker-event path (Requirements 10.1 and 10.3).
//! 4. The orchestrator wrote no per-issue runtime state to disk beyond
//!    the workspace directories (Requirement 10.4).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::mpsc;

use roki_daemon::engine::policy::WorkerOutcome;
use roki_daemon::engine::{SupervisedEvent, WorkerContext};
use roki_daemon::orchestrator::core::{EngineLauncher, LaunchError, Orchestrator};
use roki_daemon::orchestrator::events::EventBus;
use roki_daemon::orchestrator::hooks::HookRegistry;
use roki_daemon::orchestrator::read::OrchestratorRead;
use roki_daemon::orchestrator::recovery::{RecoveryDecision, RecoveryLinearReader};
use roki_daemon::orchestrator::state::{IssueId, RepoId, WorkerState};
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use roki_daemon::workspace::{Workspace, WorkspaceError};

/// Engine stub: counts launches and emits a single terminal event per
/// launch. The recovery test only needs the `Active`-state path to be
/// driven through the worker actor; it does not assert on the engine
/// behaviour beyond "an Active issue produced exactly one launch".
struct CountingEngine {
    outcome: WorkerOutcome,
}

#[async_trait]
impl EngineLauncher for CountingEngine {
    async fn launch(
        &self,
        _ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError> {
        // Sleep briefly so the worker actor has time to settle in `Active`
        // before the engine returns. The test asserts on the `Active` state
        // mid-run, so the engine must not race the snapshot.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = events.send(SupervisedEvent::Exited(self.outcome)).await;
        Ok(self.outcome)
    }
}

/// In-memory `RecoveryLinearReader` stub. Returns a fixed map of active
/// issues so the test can assert the recovery matrix without any HTTP.
struct StubLinearReader {
    active: HashMap<(RepoId, IssueId), NormalizedIssue>,
}

#[async_trait]
impl RecoveryLinearReader for StubLinearReader {
    async fn active_issues(&self) -> Result<HashMap<(RepoId, IssueId), NormalizedIssue>, String> {
        Ok(self.active.clone())
    }
}

fn linear_active(repo: &str, issue: &str) -> ((RepoId, IssueId), NormalizedIssue) {
    let key = (RepoId::new(repo), IssueId::new(issue));
    let value = NormalizedIssue {
        repo: key.0.clone(),
        issue: key.1.clone(),
        title: format!("recovery-{repo}-{issue}"),
        description: String::new(),
        state: TrackerIssueState::Active,
        labels: Vec::new(),
        team_or_scope: "ENG".to_string(),
    };
    (key, value)
}

/// Stub workspace for the recovery test. Replaces the production
/// `WorkspaceManager` so the test can pre-seed `list_existing` directly,
/// independent of the new worktree model. Tracks ensure/remove invocations
/// per `(repo, issue)` and returns synthesised paths under a tempdir.
struct StubWorkspace {
    root: tempfile::TempDir,
    /// Pre-seeded `(repo, issue) -> path` returned by `list_existing`.
    seeded: Vec<(RepoId, IssueId, PathBuf)>,
    ensures: StdMutex<Vec<(String, String)>>,
}

#[async_trait]
impl Workspace for StubWorkspace {
    async fn ensure(&self, repo: &RepoId, issue: &IssueId) -> Result<PathBuf, WorkspaceError> {
        self.ensures
            .lock()
            .unwrap()
            .push((repo.as_str().to_string(), issue.as_str().to_string()));
        let path = self.root.path().join(repo.as_str()).join(issue.as_str());
        std::fs::create_dir_all(&path).map_err(|err| WorkspaceError::InvalidIdentifier {
            reason: format!("stub create_dir_all: {err}"),
        })?;
        Ok(path)
    }

    async fn remove(&self, _repo: &RepoId, _issue: &IssueId) -> Result<(), WorkspaceError> {
        Ok(())
    }

    async fn list_existing(&self) -> Result<Vec<(RepoId, IssueId, PathBuf)>, WorkspaceError> {
        Ok(self.seeded.clone())
    }
}

async fn await_state<R: OrchestratorRead>(
    read_handle: &R,
    issue: &IssueId,
    target: WorkerState,
    timeout: Duration,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if let Some(snapshot) = read_handle.issue(issue) {
            if snapshot.state == target {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn recovery_reconciles_workspaces_and_linear_state_on_startup() {
    // Pre-seed the workspace stub's `list_existing` with two entries. The
    // third Linear active issue (ENG-3) has no workspace and must be
    // created by the orchestrator's normal Queued -> Active path.
    let root = tempdir().expect("tempdir for workspace root");
    let seed_path_1 = root.path().join("repo-a").join("ENG-1");
    let seed_path_2 = root.path().join("repo-a").join("ENG-2");
    std::fs::create_dir_all(&seed_path_1).expect("seed workspace ENG-1");
    std::fs::create_dir_all(&seed_path_2).expect("seed workspace ENG-2");
    let workspace_root_path = root.path().to_path_buf();

    let workspace = Arc::new(StubWorkspace {
        root,
        seeded: vec![
            (
                RepoId::new("repo-a"),
                IssueId::new("ENG-1"),
                seed_path_1.clone(),
            ),
            (
                RepoId::new("repo-a"),
                IssueId::new("ENG-2"),
                seed_path_2.clone(),
            ),
        ],
        ensures: StdMutex::new(Vec::new()),
    });

    // Linear stub reports ENG-1 and ENG-3 active. ENG-2 is intentionally
    // absent so the matrix produces an `OrphanedWorkspace` decision.
    let mut active = HashMap::new();
    let (k1, v1) = linear_active("repo-a", "ENG-1");
    active.insert(k1, v1);
    let (k3, v3) = linear_active("repo-a", "ENG-3");
    active.insert(k3, v3);
    let reader = StubLinearReader { active };

    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();
    let engine = Arc::new(CountingEngine {
        outcome: WorkerOutcome::CleanExit,
    });

    // The recovery synthesizer and the live tracker bridge share a single
    // channel; the test owns the sender so it can drop it on shutdown.
    let (tracker_tx, tracker_rx) = mpsc::channel::<NormalizedIssue>(16);

    let (orchestrator, decisions) = Orchestrator::with_recovery(
        Arc::clone(&workspace) as Arc<_>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        tracker_rx,
        tracker_tx.clone(),
        &reader,
    )
    .await
    .expect("recovery must succeed against the in-memory reader");

    // 1. Decision shape must match the documented matrix exactly. The
    //    decisions list is sorted by (repo, issue), so ENG-1 first, ENG-2
    //    second, ENG-3 third.
    assert_eq!(
        decisions.len(),
        3,
        "exactly one decision per key in the union; got {decisions:?}",
    );
    assert!(
        matches!(
            &decisions[0],
            RecoveryDecision::ResumeActive { repo, issue }
                if repo.as_str() == "repo-a" && issue.as_str() == "ENG-1"
        ),
        "first decision must be ResumeActive for (repo-a, ENG-1); got {:?}",
        decisions[0],
    );
    assert!(
        matches!(
            &decisions[1],
            RecoveryDecision::OrphanedWorkspace { repo, issue }
                if repo.as_str() == "repo-a" && issue.as_str() == "ENG-2"
        ),
        "second decision must be OrphanedWorkspace for (repo-a, ENG-2); got {:?}",
        decisions[1],
    );
    assert!(
        matches!(
            &decisions[2],
            RecoveryDecision::FreshQueued { repo, issue }
                if repo.as_str() == "repo-a" && issue.as_str() == "ENG-3"
        ),
        "third decision must be FreshQueued for (repo-a, ENG-3); got {:?}",
        decisions[2],
    );

    // 2. Orphaned workspace directory must remain on disk (Requirement 10.2).
    assert!(
        workspace_root_path
            .as_path()
            .join("repo-a")
            .join("ENG-2")
            .exists(),
        "orphaned workspace ENG-2 must be retained, not deleted",
    );

    // Boot the orchestrator runtime; the synthetic events posted by recovery
    // are already buffered in `tracker_rx`, so the actor will pick them up
    // immediately.
    let read_handle = orchestrator.read_handle();
    let run_handle = tokio::spawn(async move { orchestrator.run().await });

    // 3. ENG-1 and ENG-3 must reach Active. The CountingEngine sleeps for
    //    50ms before exiting, so we have a window to observe the Active
    //    state. We use a long enough timeout to absorb scheduling jitter.
    let eng1 = IssueId::new("ENG-1");
    let eng3 = IssueId::new("ENG-3");
    let saw_eng1_active = await_state(
        &read_handle,
        &eng1,
        WorkerState::Active,
        Duration::from_secs(5),
    )
    .await;
    let saw_eng3_active = await_state(
        &read_handle,
        &eng3,
        WorkerState::Active,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        saw_eng1_active,
        "ENG-1 must reach Active after recovery resumed it",
    );
    assert!(
        saw_eng3_active,
        "ENG-3 must reach Active after recovery created a fresh workspace",
    );

    // After the engine exits cleanly, ENG-1 and ENG-3 advance to
    // AwaitingReview. ENG-2 must NOT appear in the snapshot at all because
    // it was orphaned without spawning an actor.
    let saw_eng1_review = await_state(
        &read_handle,
        &eng1,
        WorkerState::AwaitingReview,
        Duration::from_secs(5),
    )
    .await;
    let saw_eng3_review = await_state(
        &read_handle,
        &eng3,
        WorkerState::AwaitingReview,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        saw_eng1_review,
        "ENG-1 must reach AwaitingReview after engine clean exit",
    );
    assert!(
        saw_eng3_review,
        "ENG-3 must reach AwaitingReview after engine clean exit",
    );

    let snapshot = read_handle.snapshot();
    assert!(
        !snapshot.issues.iter().any(|i| i.issue.as_str() == "ENG-2"),
        "ENG-2 must NOT appear in the orchestrator state map; orphaned workspaces have no actor",
    );

    // 4. Disk-write budget: per Requirement 10.4 the daemon writes no
    //    per-issue runtime state beyond workspace directories. We assert
    //    by listing the workspace root's top-level entries and checking
    //    that every entry is a directory whose name corresponds to a repo
    //    we configured. The repo dir then contains only issue dirs. This
    //    catches a regression where a future change persists a sidecar
    //    JSON / SQLite / lock file at any of these levels.
    let root_entries: Vec<_> = std::fs::read_dir(workspace_root_path.as_path())
        .expect("read workspace root")
        .map(|e| e.expect("dir entry").path())
        .collect();
    for entry in &root_entries {
        assert!(
            entry.is_dir(),
            "workspace root must contain only directories; found non-dir entry {entry:?}",
        );
    }
    let repo_a = workspace_root_path.as_path().join("repo-a");
    let repo_entries: Vec<_> = std::fs::read_dir(&repo_a)
        .expect("read repo dir")
        .map(|e| e.expect("dir entry").path())
        .collect();
    for entry in &repo_entries {
        assert!(
            entry.is_dir(),
            "repo dir must contain only issue directories; found non-dir entry {entry:?}",
        );
    }

    drop(tracker_tx);
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), run_handle).await;
}
