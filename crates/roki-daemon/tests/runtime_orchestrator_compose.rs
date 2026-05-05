//! Task 10.1.2 integration test: actor map composition via runtime.
//!
//! Asserts that `runtime::compose_for_test` assembles the per-issue actor map
//! around the stub engine seams, and that pushing a synthetic
//! `ActorMessage::TrackerAdmit { issue, mode }` into the inbox spawns a
//! single per-issue actor whose `OrchestratorEngine::launch` is invoked
//! exactly once.
//!
//! Spec refs: requirements.md Req 7.1, 7.3, 13.1, 13.2; design.md "Daemon
//! bootstrap" steps 8 + 11.

mod common;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use roki_daemon::orchestrator::core::ActorMessage;
use roki_daemon::orchestrator::escalation::EscalationQueue;
use roki_daemon::orchestrator::read::{ActorSnapshot, OrchestratorRead, OrchestratorReadHandle};
use roki_daemon::orchestrator::state::{IssueId, Mode};
use roki_daemon::runtime::OrchestratorInbox;
use roki_daemon::runtime::testing::{RuntimeTestSeams, compose_for_test};

use common::{StubEngine, StubPhaseEngine, StubSessionDirs, StubWorktree};

/// Test-local harness that keeps the concrete stub handles alongside the
/// composed orchestrator, so assertions can read the stubs' recording
/// fields without re-downcasting from the trait objects.
struct TestHarness {
    engine: Arc<StubEngine>,
    inbox: OrchestratorInbox,
    /// Shared state map; held so future regression tests can assert on
    /// snapshot rows directly without going through `OrchestratorRead`.
    #[allow(dead_code)]
    state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>>,
    read_handle: Arc<OrchestratorReadHandle>,
    #[allow(dead_code)]
    escalations: Arc<EscalationQueue>,
    /// Lifetime anchors for the temp dirs the stubs hand out.
    _tempdirs: Vec<tempfile::TempDir>,
}

/// Build the orchestrator composition around recording stubs and surface
/// the concrete stub handles + composition outputs needed for assertions.
fn compose_with_stubs() -> TestHarness {
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

    TestHarness {
        engine,
        inbox: harness.inbox,
        state_map: harness.state_map,
        read_handle: harness.read_handle,
        escalations: harness.escalations,
        _tempdirs: vec![session_root, worktree_root],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admit_via_inbox_invokes_orchestrator_engine_exactly_once() {
    let harness = compose_with_stubs();

    // Pre-seed the engine with an empty action stream so the spawned
    // session immediately exits its action loop without driving phases.
    // (An empty action stream lets `drain_actions` observe a closed action
    // channel; the actor maps that to `Inactive(orchestrator_crash)` per
    // the documented orchestrator-dead handling, which is irrelevant to
    // this test — we only assert that `launch` was invoked exactly once.)
    harness
        .engine
        .launch_streams
        .lock()
        .await
        .push_back(VecDeque::new());

    let issue = IssueId::from("ENG-1");
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

    // Wait for the engine.launch counter to reach 1 — the load-bearing
    // observation for this task. The actor records mode + admission state
    // in the snapshot row before invoking `launch`, so reading the snapshot
    // first would race the launch await.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count = *harness.engine.launch_count.lock().await;
        if count >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for engine.launch invocation");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Engine.launch was invoked exactly once for ENG-1.
    let count = *harness.engine.launch_count.lock().await;
    assert_eq!(count, 1, "engine.launch must be invoked exactly once");

    // The launched mode is captured by the stub engine for later assertion.
    let modes = harness.engine.launch_modes.lock().await.clone();
    assert_eq!(modes, vec![Mode::SpecDriven]);

    // The OrchestratorRead snapshot reflects the issue's row. The actor
    // may have transitioned through Pending into Inactive(orchestrator_crash)
    // because the empty action stream forces a synthetic process-exit; the
    // load-bearing assertion is that the snapshot exposes the row at all
    // and that the mode is recorded.
    let snapshot = harness.read_handle.snapshot();
    let row = snapshot
        .issues
        .iter()
        .find(|row| row.issue == issue)
        .expect("issue row in OrchestratorRead snapshot");
    assert_eq!(row.mode, Some(Mode::SpecDriven));
}

/// Drop-stub variant: dropping the harness must let any spawned actor tasks
/// wind down without panicking. Smoke-only; serves as a regression guard for
/// future shutdown wiring (Task 10.1.6).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_harness_does_not_panic_with_active_actor() {
    let harness = compose_with_stubs();
    harness
        .engine
        .launch_streams
        .lock()
        .await
        .push_back(VecDeque::new());

    harness
        .inbox
        .send(
            IssueId::from("ENG-3"),
            ActorMessage::TrackerAdmit {
                mode: Mode::SpecDriven,
                repo: None,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(harness);
    // Reaching this line without panic is the assertion.
}

