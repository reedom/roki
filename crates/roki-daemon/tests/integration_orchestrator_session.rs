//! Integration smoke tests for the orchestrator-session adapter.
//!
//! Drives `OrchestratorSessionAdapter` against the `fake_claude` example
//! binary to assert one full launch -> action -> shutdown cycle plus the
//! follow-up action after a `phase_complete` event flows back through the
//! daemon -> orchestrator stdin channel.

#![cfg(unix)]

mod common;

use std::time::Duration;

use roki_daemon::engine::claude::ClaudeBinary;
use roki_daemon::engine::orchestrator_session::action_parser::{
    ActionKind, PhaseName as ParserPhaseName,
};
use roki_daemon::engine::orchestrator_session::adapter::{
    ActionEvent, OrchestratorLaunchContext, OrchestratorSessionAdapter,
};
use roki_daemon::engine::orchestrator_session::events::{DaemonEvent, PhaseCompletePayload};
use roki_daemon::engine::phase_subprocess::catalog::PhaseName;
use roki_daemon::orchestrator::state::{IssueId, Mode};
use roki_daemon::permissions::PermissionResolver;

fn make_adapter() -> OrchestratorSessionAdapter {
    let binary =
        ClaudeBinary::discover(Some(common::fake_claude_path())).expect("fake_claude discoverable");
    let permissions = PermissionResolver::resolve_for_orchestrator(&[
        "Read".to_owned(),
        "mcp__linear*".to_owned(),
    ]);
    OrchestratorSessionAdapter::new(binary, permissions)
}

fn launch_ctx(tempdir: std::path::PathBuf) -> OrchestratorLaunchContext {
    OrchestratorLaunchContext {
        issue: IssueId::from("ENG-1"),
        mode: Mode::SpecDriven,
        session_tempdir: tempdir,
        system_prompt: "ROKI-INTEGRATION-MARKER".to_owned(),
        allowed_tools: vec!["Read".to_owned()],
        debug_sink: None,
    }
}

#[tokio::test]
async fn launch_receive_action_and_shutdown_full_cycle() {
    let tmp = tempfile::tempdir().unwrap();
    common::write_mode(tmp.path(), "single_action");

    let adapter = make_adapter();
    let mut handle = adapter
        .launch(launch_ctx(tmp.path().to_path_buf()))
        .await
        .expect("orchestrator session launch");

    let event = tokio::time::timeout(Duration::from_secs(5), handle.action_rx.recv())
        .await
        .expect("recv timeout")
        .expect("action event");
    match event {
        ActionEvent::Action(action) => {
            assert_eq!(action.action, ActionKind::RunPhase);
            assert_eq!(action.phase, Some(ParserPhaseName::Implement));
            // The fake binary echoes the system-prompt marker into its first
            // action's reason field, proving stdin-first delivery.
            assert!(action.reason.as_str().contains("ROKI-INTEGRATION-MARKER"));
        }
        other => panic!("expected Action, got {other:?}"),
    }

    let status = handle.shutdown(Some(Duration::from_secs(3))).await;
    assert!(status.success(), "fake_claude should exit cleanly on EOF");
}

#[tokio::test]
async fn phase_complete_event_drives_follow_up_action() {
    let tmp = tempfile::tempdir().unwrap();
    common::write_mode(tmp.path(), "echo_phase_complete");

    let adapter = make_adapter();
    let mut handle = adapter
        .launch(launch_ctx(tmp.path().to_path_buf()))
        .await
        .expect("orchestrator session launch");

    // Drain the first nominal action.
    let first = tokio::time::timeout(Duration::from_secs(5), handle.action_rx.recv())
        .await
        .expect("first action timeout")
        .expect("first action event");
    assert!(matches!(first, ActionEvent::Action(_)));

    // Send a phase_complete event; harness responds by emitting a follow-up
    // run_phase(open_pr) action.
    handle
        .stdin_tx
        .send(DaemonEvent::PhaseComplete(PhaseCompletePayload {
            phase: PhaseName::Implement,
            result: serde_json::json!({"subtype": "success"}),
            pr_url: None,
            review_artifact_path: None,
            classify: None,
        }))
        .await
        .expect("send phase_complete");

    let second = tokio::time::timeout(Duration::from_secs(5), handle.action_rx.recv())
        .await
        .expect("second action timeout")
        .expect("second action event");
    match second {
        ActionEvent::Action(action) => {
            assert_eq!(action.phase, Some(ParserPhaseName::OpenPr));
        }
        other => panic!("expected open_pr Action, got {other:?}"),
    }

    let _status = handle.shutdown(Some(Duration::from_secs(3))).await;
}

#[tokio::test]
async fn shutdown_completes_within_grace_window_on_idle_session() {
    let tmp = tempfile::tempdir().unwrap();
    common::write_mode(tmp.path(), "wait_for_stdin_close");

    let adapter = make_adapter();
    let handle = adapter
        .launch(launch_ctx(tmp.path().to_path_buf()))
        .await
        .expect("orchestrator session launch");

    let started = std::time::Instant::now();
    let status = handle.shutdown(Some(Duration::from_secs(3))).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "shutdown should complete inside grace ({:?})",
        started.elapsed()
    );
    assert!(status.success());
}
