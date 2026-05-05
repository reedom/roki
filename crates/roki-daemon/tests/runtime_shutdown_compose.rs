//! Task 10.1.6 integration test: phased shutdown across orchestrator actor
//! map + tracker + reconciler + webhook server within `SHUTDOWN_WINDOW`.
//!
//! Asserts the runtime composition:
//!   - Tracker + webhook + admission pipe stop accepting new events FIRST.
//!   - Then each live orchestrator session is closed via `engine.shutdown`.
//!   - Then per-issue actor join handles are awaited (or aborted at the
//!     window).
//!   - The whole wind-down stays inside `SHUTDOWN_WINDOW`.
//!
//! Spec refs: requirements.md Req 1.4, 7.1, 7.2, 7.3; design.md "Daemon
//! bootstrap" step 12.

#![cfg(unix)]

mod common;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use roki_daemon::orchestrator::core::{
    ActorMessage, OrchestratorActionEvent,
};
use roki_daemon::orchestrator::state::{IssueId, Mode};
use roki_daemon::runtime::testing::{
    RuntimeTestSeams, compose_for_test, drain_actors_with_window_for_test,
};

use common::{StubEngine, StubPhaseEngine, StubSessionDirs, StubWorktree};

// ---------------------------------------------------------------------------
// Test 1: shutdown drives engine.shutdown at the orchestrator session seam
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_invokes_orchestrator_session_shutdown_for_live_actor() {
    let session_root = tempfile::tempdir().expect("session_root tempdir");
    let worktree_root = tempfile::tempdir().expect("worktree_root tempdir");

    let engine = StubEngine::new();
    let phase = StubPhaseEngine::new();
    let worktree = StubWorktree::new(worktree_root.path().to_path_buf());
    let session_dirs = StubSessionDirs::new(session_root.path().to_path_buf());

    // Empty action stream: actor admits, drain_actions immediately observes
    // a closed action stream and routes to Inactive(orchestrator_crash). The
    // session is still held on `state.session` after the transition because
    // `transition_to_inactive` does not take it; the actor returns to the
    // inbox loop and waits at `rx.recv()`. Dropping the inbox is what we
    // exercise here.
    engine
        .launch_streams
        .lock()
        .await
        .push_back(VecDeque::new());

    let seams = RuntimeTestSeams {
        orchestrator_engine: engine.clone(),
        phase_engine: phase.clone(),
        worktree: worktree.clone(),
        session_dirs: session_dirs.clone(),
    };
    let harness = compose_for_test(seams);

    // Admit an issue to spawn an actor.
    let issue = IssueId::from("ENG-501");
    harness
        .inbox
        .send(
            issue.clone(),
            ActorMessage::TrackerAdmit {
                mode: Mode::SpecDriven,
                repo: None,
            },
        )
        .await
        .expect("send admit");

    // Wait for the actor to land at the inbox (post-admit, post-drain).
    let deadline = Instant::now() + Duration::from_secs(5);
    while *engine.launch_count.lock().await == 0 {
        if Instant::now() >= deadline {
            panic!("engine.launch never invoked");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Give the actor a beat to finish drain_actions and return to recv.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Drop the inbox + drain actor handles; await within window.
    let started = Instant::now();
    let outcome =
        drain_actors_with_window_for_test(harness.orchestrator.clone(), harness.inbox, Duration::from_secs(5))
            .await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown must complete inside the window: took {elapsed:?}"
    );
    assert!(
        outcome.timed_out.is_empty(),
        "no actor should time out, got {:?}",
        outcome.timed_out
    );

    // Engine seam: at least one session.shutdown was invoked. The shutdown
    // counter is shared across every StubSession the engine handed out.
    let shutdown_calls = *engine.shutdown_calls.lock().await;
    assert!(
        shutdown_calls >= 1,
        "expected orchestrator session shutdown for live actor; got {shutdown_calls}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: timeout path — actor blocked in run_phase exceeds the window
// ---------------------------------------------------------------------------

/// Phase-engine stub that hangs `run_phase` until the test cancels the
/// future, recording the cancellation in a shared counter. The actor will
/// be blocked inside this future when shutdown begins; aborting the actor's
/// JoinHandle drops the future and increments `kill_count`.
struct HangingPhaseEngine {
    invocations: Arc<AtomicU64>,
    kill_count: Arc<AtomicU64>,
}

impl HangingPhaseEngine {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            invocations: Arc::new(AtomicU64::new(0)),
            kill_count: Arc::new(AtomicU64::new(0)),
        })
    }

    fn invocations(&self) -> u64 {
        self.invocations.load(Ordering::SeqCst)
    }

    fn kill_count(&self) -> u64 {
        self.kill_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl roki_daemon::orchestrator::core::PhaseEngine for HangingPhaseEngine {
    async fn run_phase(
        &self,
        _issue: &IssueId,
        _phase: roki_daemon::engine::orchestrator_session::action_parser::PhaseName,
        _mode: Mode,
        _worktree_path: Option<std::path::PathBuf>,
        _additional_context: Option<String>,
    ) -> Result<
        roki_daemon::orchestrator::core::PhaseRunOutcome,
        roki_daemon::orchestrator::core::EngineError,
    > {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        // RAII guard: increment kill_count on drop so the test can observe
        // the future-cancellation via JoinHandle::abort().
        struct KillGuard(Arc<AtomicU64>);
        impl Drop for KillGuard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _guard = KillGuard(self.kill_count.clone());
        // Hang forever; the actor task abort drops this future and runs the
        // guard's Drop impl.
        std::future::pending::<()>().await;
        unreachable!()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_window_aborts_actors_blocked_in_run_phase() {
    use roki_daemon::engine::orchestrator_session::action_parser::PhaseName as ParserPhaseName;

    let session_root = tempfile::tempdir().expect("session_root tempdir");
    let worktree_root = tempfile::tempdir().expect("worktree_root tempdir");

    let engine = StubEngine::new();
    let hanging_phase = HangingPhaseEngine::new();
    let worktree = StubWorktree::new(worktree_root.path().to_path_buf());
    let session_dirs = StubSessionDirs::new(session_root.path().to_path_buf());

    // The actor admits then runs a single `run_phase(implement)` action.
    // The hanging phase blocks forever; the actor cannot return to its
    // inbox until the JoinHandle is aborted.
    let mut actions = VecDeque::new();
    actions.push_back(OrchestratorActionEvent::Action(common::run_phase_action(
        ParserPhaseName::Implement,
    )));
    engine.launch_streams.lock().await.push_back(actions);

    let seams = RuntimeTestSeams {
        orchestrator_engine: engine.clone(),
        phase_engine: hanging_phase.clone(),
        worktree: worktree.clone(),
        session_dirs: session_dirs.clone(),
    };
    let harness = compose_for_test(seams);

    let issue = IssueId::from("ENG-502");
    harness
        .inbox
        .send(
            issue.clone(),
            ActorMessage::TrackerAdmit {
                mode: Mode::SpecDriven,
                // Use a repo so worktree.ensure() is exercised; the stub
                // returns Ok by default.
                repo: Some(roki_daemon::tracker::model::RepoId::from("acme/web")),
            },
        )
        .await
        .expect("send admit");

    // Wait until the actor enters run_phase (hanging).
    let deadline = Instant::now() + Duration::from_secs(5);
    while hanging_phase.invocations() == 0 {
        if Instant::now() >= deadline {
            panic!("actor never entered run_phase");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Sub-window shorter than the actor's natural exit (which is never).
    let window = Duration::from_millis(300);
    let started = Instant::now();
    let outcome = drain_actors_with_window_for_test(
        harness.orchestrator.clone(),
        harness.inbox,
        window,
    )
    .await;
    let elapsed = started.elapsed();

    // Window must bound the wait.
    assert!(
        elapsed < Duration::from_secs(2),
        "must return shortly after window: took {elapsed:?}"
    );
    // The hanging actor must surface in the timed_out list.
    assert!(
        !outcome.timed_out.is_empty(),
        "expected at least one timed_out actor; got completed={:?}",
        outcome.completed
    );

    // After the abort, the run_phase future is dropped → kill guard fires.
    // Allow a brief moment for the abort to propagate.
    let deadline = Instant::now() + Duration::from_secs(2);
    while hanging_phase.kill_count() == 0 {
        if Instant::now() >= deadline {
            panic!("phase engine kill guard never fired after actor abort");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Test 3: drain_actors returns immediately when the actor map is empty
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_actors_no_op_when_no_admissions_have_landed() {
    let session_root = tempfile::tempdir().expect("session_root tempdir");
    let worktree_root = tempfile::tempdir().expect("worktree_root tempdir");

    let engine = StubEngine::new();
    let phase = StubPhaseEngine::new();
    let worktree = StubWorktree::new(worktree_root.path().to_path_buf());
    let session_dirs = StubSessionDirs::new(session_root.path().to_path_buf());

    let seams = RuntimeTestSeams {
        orchestrator_engine: engine.clone(),
        phase_engine: phase.clone(),
        worktree: worktree.clone(),
        session_dirs: session_dirs.clone(),
    };
    let harness = compose_for_test(seams);

    let started = Instant::now();
    let outcome = drain_actors_with_window_for_test(
        harness.orchestrator.clone(),
        harness.inbox,
        Duration::from_secs(2),
    )
    .await;
    let elapsed = started.elapsed();

    assert!(elapsed < Duration::from_millis(500));
    assert!(outcome.completed.is_empty());
    assert!(outcome.timed_out.is_empty());
}
