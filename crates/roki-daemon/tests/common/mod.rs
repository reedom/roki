//! Shared helpers for the section-12 + section-13 integration tests.
//!
//! Centralizes:
//! - the `fake_claude` example-binary build + path resolution + per-CWD
//!   `.fake_claude_mode` selector,
//! - reusable stub implementations of the orchestrator core's engine seams
//!   (`OrchestratorEngine`, `PhaseEngine`, `WorktreeOps`, `SessionDirOps`)
//!   so each section-13 e2e test exercises the same integrated pipeline
//!   shape without duplicating the StubEngine ceremony from
//!   `orchestrator::core` unit tests.
//!
//! These helpers do NOT exercise `runtime::run_with_shutdown` end-to-end —
//! `runtime.rs` does not yet wire the orchestrator + tracker + recovery
//! pipeline into the bootstrap path. The section-13 tests therefore drive
//! the orchestrator core's stub-engine seam directly. See each test file's
//! header for the runtime-wiring TODO that would let it lift the seam-level
//! framing.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use roki_daemon::engine::orchestrator_session::action_parser::{
    ActionKind, BoundedString200, OrchestratorAction, Outcome, PhaseName,
};
use roki_daemon::engine::orchestrator_session::events::{
    DaemonEvent, NoncleanClassification, PhaseCompletePayload, PhaseNoncleanPayload,
};
use roki_daemon::orchestrator::core::{
    DeliveryError, EngineError, Orchestrator, OrchestratorActionEvent, OrchestratorDeps,
    OrchestratorEngine, OrchestratorSessionLike, PhaseEngine, PhaseRunOutcome, SessionDirError,
    SessionDirOps, WorktreeOpError, WorktreeOps,
};
use roki_daemon::orchestrator::escalation::EscalationQueue;
use roki_daemon::orchestrator::events::EventBus;
use roki_daemon::orchestrator::hooks::SubscriberHooks;
use roki_daemon::orchestrator::read::ActorSnapshot;
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode, WorkerState};
use roki_daemon::tracker::model::RepoId;
use tokio::sync::{Mutex as AsyncMutex, broadcast};

// ---------------------------------------------------------------------------
// fake_claude binary helpers
// ---------------------------------------------------------------------------

/// Build (once per process) the `fake_claude` example binary and return its
/// path under `target/debug/examples`.
pub fn fake_claude_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let status = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "--example",
                "fake_claude",
                "-p",
                "roki-daemon",
            ])
            .status()
            .expect("invoke cargo build --example fake_claude");
        assert!(status.success(), "fake_claude example build failed");
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .ancestors()
            .nth(2)
            .expect("workspace root is two levels above the daemon manifest")
            .to_path_buf();
        workspace_root
            .join("target")
            .join("debug")
            .join("examples")
            .join("fake_claude")
    })
    .as_path()
}

/// Write the `.fake_claude_mode` selector file the harness reads from CWD.
pub fn write_mode(dir: &Path, mode: &str) {
    std::fs::write(dir.join(".fake_claude_mode"), mode).expect("write fake_claude_mode");
}

// ---------------------------------------------------------------------------
// Action-stream builders
// ---------------------------------------------------------------------------

pub fn reason(s: &str) -> BoundedString200 {
    BoundedString200::new(s).expect("reason fits 200 chars")
}

pub fn run_phase_action(phase: PhaseName) -> OrchestratorAction {
    OrchestratorAction {
        action: ActionKind::RunPhase,
        phase: Some(phase),
        additional_context: None,
        outcome: None,
        linear_writes: None,
        reason: reason("nominate"),
    }
}

pub fn run_phase_action_with_context(phase: PhaseName, ctx: &str) -> OrchestratorAction {
    OrchestratorAction {
        action: ActionKind::RunPhase,
        phase: Some(phase),
        additional_context: Some(ctx.to_owned()),
        outcome: None,
        linear_writes: None,
        reason: reason("nominate"),
    }
}

pub fn stop_action(outcome: Outcome) -> OrchestratorAction {
    OrchestratorAction {
        action: ActionKind::Stop,
        phase: None,
        additional_context: None,
        outcome: Some(outcome),
        linear_writes: None,
        reason: reason("stop"),
    }
}

pub fn phase_complete_event(phase: PhaseName) -> DaemonEvent {
    DaemonEvent::PhaseComplete(PhaseCompletePayload {
        phase,
        result: serde_json::json!({"subtype": "success"}),
        pr_url: None,
        review_artifact_path: None,
        classify: None,
    })
}

pub fn phase_nonclean_event(
    phase: PhaseName,
    classification: NoncleanClassification,
) -> DaemonEvent {
    DaemonEvent::PhaseNonclean(PhaseNoncleanPayload {
        phase,
        classification,
        raw_subtype: None,
        additional_context: None,
    })
}

// ---------------------------------------------------------------------------
// Stub engines
// ---------------------------------------------------------------------------

/// Test stub for an orchestrator session: pre-seeded action queue +
/// records every delivered DaemonEvent.
pub struct StubSession {
    pub actions: AsyncMutex<VecDeque<OrchestratorActionEvent>>,
    pub delivered: Arc<AsyncMutex<Vec<DaemonEvent>>>,
    pub shutdown_calls: Arc<AsyncMutex<u32>>,
    /// When set, deliver fails — used to model an orchestrator stdin that
    /// has closed (e.g. crash mid-flight).
    pub fail_deliver: Arc<AsyncMutex<bool>>,
}

#[async_trait]
impl OrchestratorSessionLike for StubSession {
    async fn deliver(&self, event: DaemonEvent) -> Result<(), DeliveryError> {
        if *self.fail_deliver.lock().await {
            return Err(DeliveryError::Closed);
        }
        self.delivered.lock().await.push(event);
        Ok(())
    }

    async fn next_action(&mut self) -> Option<OrchestratorActionEvent> {
        self.actions.lock().await.pop_front()
    }

    async fn shutdown(self: Box<Self>, _grace: Option<Duration>) {
        *self.shutdown_calls.lock().await += 1;
    }
}

pub struct StubEngine {
    /// One-per-launch queue of pre-canned action streams. Each call to
    /// [`launch`] takes the front-of-queue stream. Empty queue yields an
    /// empty stream (orchestrator session that immediately exits).
    pub launch_streams: AsyncMutex<VecDeque<VecDeque<OrchestratorActionEvent>>>,
    pub delivered: Arc<AsyncMutex<Vec<DaemonEvent>>>,
    pub shutdown_calls: Arc<AsyncMutex<u32>>,
    pub launch_count: AsyncMutex<u32>,
    pub launch_modes: AsyncMutex<Vec<Mode>>,
    /// Captures the rendered system prompt passed to each `launch` call so
    /// tests can assert prompt content (e.g. recovery rendering the
    /// recomputed `mode` per Req 8.5 / 10.2).
    pub launch_prompts: AsyncMutex<Vec<String>>,
    pub fail_deliver: Arc<AsyncMutex<bool>>,
    /// When set, the next launch returns this error and decrements no
    /// streams from the queue.
    pub launch_error: AsyncMutex<Option<EngineError>>,
}

impl StubEngine {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            launch_streams: AsyncMutex::new(VecDeque::new()),
            delivered: Arc::new(AsyncMutex::new(Vec::new())),
            shutdown_calls: Arc::new(AsyncMutex::new(0)),
            launch_count: AsyncMutex::new(0),
            launch_modes: AsyncMutex::new(Vec::new()),
            launch_prompts: AsyncMutex::new(Vec::new()),
            fail_deliver: Arc::new(AsyncMutex::new(false)),
            launch_error: AsyncMutex::new(None),
        })
    }

    pub async fn push_stream(&self, stream: Vec<OrchestratorActionEvent>) {
        self.launch_streams
            .lock()
            .await
            .push_back(stream.into_iter().collect());
    }
}

#[async_trait]
impl OrchestratorEngine for StubEngine {
    async fn launch(
        &self,
        _issue: &IssueId,
        mode: Mode,
        system_prompt: String,
    ) -> Result<Box<dyn OrchestratorSessionLike>, EngineError> {
        if let Some(err) = self.launch_error.lock().await.take() {
            return Err(err);
        }
        *self.launch_count.lock().await += 1;
        self.launch_modes.lock().await.push(mode);
        self.launch_prompts.lock().await.push(system_prompt);
        let actions = self
            .launch_streams
            .lock()
            .await
            .pop_front()
            .unwrap_or_default();
        Ok(Box::new(StubSession {
            actions: AsyncMutex::new(actions),
            delivered: self.delivered.clone(),
            shutdown_calls: self.shutdown_calls.clone(),
            fail_deliver: self.fail_deliver.clone(),
        }))
    }
}

#[derive(Clone, Debug)]
pub struct PhaseInvocation {
    pub phase: PhaseName,
    pub mode: Mode,
    pub worktree_path: Option<PathBuf>,
    pub additional_context: Option<String>,
}

pub struct StubPhaseEngine {
    pub canned: AsyncMutex<VecDeque<PhaseRunOutcome>>,
    pub invocations: AsyncMutex<Vec<PhaseInvocation>>,
}

impl StubPhaseEngine {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            canned: AsyncMutex::new(VecDeque::new()),
            invocations: AsyncMutex::new(Vec::new()),
        })
    }

    pub async fn push_outcome(&self, outcome: PhaseRunOutcome) {
        self.canned.lock().await.push_back(outcome);
    }
}

#[async_trait]
impl PhaseEngine for StubPhaseEngine {
    async fn run_phase(
        &self,
        _issue: &IssueId,
        phase: PhaseName,
        mode: Mode,
        worktree_path: Option<PathBuf>,
        additional_context: Option<String>,
    ) -> Result<PhaseRunOutcome, EngineError> {
        self.invocations.lock().await.push(PhaseInvocation {
            phase,
            mode,
            worktree_path: worktree_path.clone(),
            additional_context,
        });
        Ok(self
            .canned
            .lock()
            .await
            .pop_front()
            .unwrap_or(PhaseRunOutcome::Translated(phase_complete_event(phase))))
    }
}

pub struct StubWorktree {
    pub ensure_calls: AsyncMutex<Vec<(IssueId, RepoId)>>,
    pub cleanup_calls: AsyncMutex<Vec<IssueId>>,
    pub ensure_results: AsyncMutex<VecDeque<Result<PathBuf, WorktreeOpError>>>,
    pub cleanup_results: AsyncMutex<VecDeque<Result<Vec<PathBuf>, WorktreeOpError>>>,
    pub default_root: PathBuf,
}

impl StubWorktree {
    pub fn new(default_root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            ensure_calls: AsyncMutex::new(Vec::new()),
            cleanup_calls: AsyncMutex::new(Vec::new()),
            ensure_results: AsyncMutex::new(VecDeque::new()),
            cleanup_results: AsyncMutex::new(VecDeque::new()),
            default_root,
        })
    }
}

#[async_trait]
impl WorktreeOps for StubWorktree {
    async fn ensure(
        &self,
        issue: &IssueId,
        repo_id: &RepoId,
    ) -> Result<PathBuf, WorktreeOpError> {
        self.ensure_calls
            .lock()
            .await
            .push((issue.clone(), repo_id.clone()));
        if let Some(canned) = self.ensure_results.lock().await.pop_front() {
            return canned;
        }
        let path = self.default_root.join(format!("{issue}"));
        // Materialize so cleanup observation tests can assert the path
        // existed before cleanup ran.
        let _ = std::fs::create_dir_all(&path);
        Ok(path)
    }

    async fn cleanup(&self, issue: &IssueId) -> Result<Vec<PathBuf>, WorktreeOpError> {
        self.cleanup_calls.lock().await.push(issue.clone());
        if let Some(canned) = self.cleanup_results.lock().await.pop_front() {
            return canned;
        }
        let path = self.default_root.join(format!("{issue}"));
        let _ = std::fs::remove_dir_all(&path);
        Ok(vec![path])
    }
}

pub struct StubSessionDirs {
    pub ensure_calls: Mutex<Vec<IssueId>>,
    pub remove_calls: Mutex<Vec<IssueId>>,
    pub ensure_fail: Mutex<Option<String>>,
    pub default_root: PathBuf,
}

impl StubSessionDirs {
    pub fn new(default_root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            ensure_calls: Mutex::new(Vec::new()),
            remove_calls: Mutex::new(Vec::new()),
            ensure_fail: Mutex::new(None),
            default_root,
        })
    }
}

impl SessionDirOps for StubSessionDirs {
    fn ensure(&self, issue: &IssueId) -> Result<PathBuf, SessionDirError> {
        self.ensure_calls.lock().unwrap().push(issue.clone());
        if let Some(err) = self.ensure_fail.lock().unwrap().take() {
            return Err(SessionDirError::Other(err));
        }
        let path = self.default_root.join(format!("{issue}"));
        let _ = std::fs::create_dir_all(&path);
        Ok(path)
    }

    fn remove(&self, issue: &IssueId) -> Result<(), SessionDirError> {
        self.remove_calls.lock().unwrap().push(issue.clone());
        let path = self.default_root.join(format!("{issue}"));
        let _ = std::fs::remove_dir_all(&path);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Harness composition
// ---------------------------------------------------------------------------

/// One assembled orchestrator-test harness with all stub engines pre-wired
/// + read accessors for assertions.
pub struct OrchHarness {
    pub orchestrator: Arc<Orchestrator>,
    pub engine: Arc<StubEngine>,
    pub phase: Arc<StubPhaseEngine>,
    pub worktree: Arc<StubWorktree>,
    pub session_dirs: Arc<StubSessionDirs>,
    pub event_bus: EventBus,
    pub escalations: Arc<EscalationQueue>,
    pub state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>>,
    /// TempDirs we keep alive so paths the stubs hand out remain valid for
    /// the test's lifetime.
    _tempdirs: Vec<tempfile::TempDir>,
}

impl OrchHarness {
    pub fn new() -> Self {
        let session_root = tempfile::tempdir().expect("session_root tempdir");
        let worktree_root = tempfile::tempdir().expect("worktree_root tempdir");
        let engine = StubEngine::new();
        let phase = StubPhaseEngine::new();
        let worktree = StubWorktree::new(worktree_root.path().to_path_buf());
        let session_dirs = StubSessionDirs::new(session_root.path().to_path_buf());
        let event_bus = EventBus::with_capacity(256);
        let escalations = Arc::new(EscalationQueue::new());
        let state_map = Arc::new(RwLock::new(HashMap::new()));

        let deps = OrchestratorDeps {
            orchestrator_engine: engine.clone(),
            phase_engine: phase.clone(),
            worktree: worktree.clone(),
            session_dirs: session_dirs.clone(),
            event_bus: event_bus.clone(),
            hooks: Arc::new(SubscriberHooks::new()),
            escalations: escalations.clone(),
            state_map: state_map.clone(),
        };

        Self {
            orchestrator: Arc::new(Orchestrator::new(deps)),
            engine,
            phase,
            worktree,
            session_dirs,
            event_bus,
            escalations,
            state_map,
            _tempdirs: vec![session_root, worktree_root],
        }
    }

    /// Subscribe to the event bus before sending any messages so the
    /// receiver sees every published transition.
    pub fn subscribe(
        &self,
    ) -> broadcast::Receiver<roki_daemon::orchestrator::state::TransitionEvent> {
        self.event_bus.subscribe()
    }

    /// Block until the per-issue snapshot reaches a state matching the
    /// predicate. Returns the snapshot or panics on timeout.
    pub async fn wait_for<F>(&self, issue: &IssueId, predicate: F) -> ActorSnapshot
    where
        F: Fn(&WorkerState) -> bool,
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let map = self.state_map.read().unwrap();
                if let Some(snap) = map.get(issue) {
                    if predicate(&snap.state) {
                        return snap.clone();
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                let map = self.state_map.read().unwrap();
                let observed = map.get(issue).map(|s| s.state.clone());
                panic!(
                    "wait_for timeout for {issue}: last observed state = {observed:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn wait_for_inactive(
        &self,
        issue: &IssueId,
        reason: InactiveReason,
    ) -> ActorSnapshot {
        self.wait_for(issue, move |state| {
            matches!(state, WorkerState::Inactive(r) if *r == reason)
        })
        .await
    }
}
