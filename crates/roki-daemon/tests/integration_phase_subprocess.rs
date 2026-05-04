//! Integration smoke tests for the phase subprocess adapter.
//!
//! Drives `PhaseSubprocessAdapter` against the `fake_claude` example binary
//! to assert the success path produces a typed `Result(success)` event and
//! that an unknown terminal subtype is forwarded to the typed exit
//! translator with `raw_subtype` preserved verbatim.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use roki_daemon::engine::claude::ClaudeBinary;
use roki_daemon::engine::orchestrator_session::events::{
    DaemonEvent, NoncleanClassification, PhaseCompletePayload,
};
use roki_daemon::engine::phase_subprocess::adapter::PhaseSubprocessAdapter;
use roki_daemon::engine::phase_subprocess::catalog::{
    PhaseLaunchContext, PhaseName, WorkflowPolicyHandle,
};
use roki_daemon::engine::phase_subprocess::exit::{
    ExitOutcome, ExitTranslationInputs, translate_exit,
};
use roki_daemon::engine::stream::StreamLine;
use roki_daemon::orchestrator::state::{IssueId, Mode};
use roki_daemon::permissions::{PermissionResolver, PermissionStrategy};
use roki_daemon::workflow::schema::{OrchestratorConfig, WorkflowPolicy};
use tokio::sync::oneshot;

fn baseline_policy() -> WorkflowPolicy {
    let mut blocks = BTreeMap::new();
    blocks.insert(
        "prompt_template_open_pr".to_owned(),
        "open_pr {{ issue }}\n".to_owned(),
    );
    blocks.insert(
        "prompt_template_implement_direct".to_owned(),
        "impl {{ issue }}\n".to_owned(),
    );
    blocks.insert(
        "prompt_template_validate_direct".to_owned(),
        "validate {{ issue }}\n".to_owned(),
    );
    WorkflowPolicy {
        orchestrator: OrchestratorConfig::default(),
        phases: BTreeMap::new(),
        server: serde_json::Value::Object(Default::default()),
        blocks,
        raw_unknowns: serde_json::Value::Object(Default::default()),
    }
}

async fn spawn_phase_with_mode(
    mode: &str,
    phase: PhaseName,
    ticket_mode: Mode,
) -> (
    roki_daemon::engine::phase_subprocess::adapter::PhaseProcessHandle,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    common::write_mode(tmp.path(), mode);
    std::fs::write(tmp.path().join("settings.json"), b"{}").unwrap();

    let bin = ClaudeBinary::discover(Some(common::fake_claude_path())).unwrap();
    let perms = PermissionResolver::with_settings_path(
        tmp.path().join("settings.json"),
        vec!["Read".to_owned()],
    );
    let adapter = PhaseSubprocessAdapter::new(bin, perms);

    let policy: WorkflowPolicyHandle = Arc::new(baseline_policy());
    let worktree = if matches!(phase, PhaseName::Classify) {
        None
    } else {
        Some(tmp.path().join("wt"))
    };

    let ctx = PhaseLaunchContext {
        issue: IssueId::from("ENG-T"),
        phase,
        mode: ticket_mode,
        additional_context: None,
        worktree_path: worktree,
        session_tempdir: tmp.path().to_path_buf(),
        max_turns: 0,
        workflow_policy: policy,
        permission_strategy: PermissionStrategy::SettingsAllowlist {
            settings_path: tmp.path().join("settings.json"),
        },
        allowed_tools: vec!["Read".to_owned()],
    };
    let handle = adapter.spawn(ctx, None).await.expect("phase spawn");
    (handle, tmp)
}

#[tokio::test]
async fn phase_success_emits_terminal_result_then_translates_to_phase_complete() {
    let (mut handle, _tmp) =
        spawn_phase_with_mode("phase_success", PhaseName::OpenPr, Mode::SpecDriven).await;

    // Stream side: the harness emits one terminal `result` line.
    let stream_event = tokio::time::timeout(Duration::from_secs(5), handle.stream_rx.recv())
        .await
        .expect("stream rx timeout")
        .expect("stream rx closed");
    match stream_event {
        StreamLine::Result { subtype, .. } => assert_eq!(subtype, "success"),
        other => panic!("expected Result, got {other:?}"),
    }

    // Build a fresh handle for translate_exit by re-spawning so we can
    // observe the typed `PhaseComplete` payload via the public exit
    // translator.
    let (handle2, _tmp2) =
        spawn_phase_with_mode("phase_success", PhaseName::OpenPr, Mode::SpecDriven).await;
    let (_send_tt, recv_tt) = oneshot::channel();
    let inputs = ExitTranslationInputs {
        child: handle2.child,
        stream_rx: handle2.stream_rx,
        phase: PhaseName::OpenPr,
        stall_window: Duration::from_secs(10),
        tracker_terminal_signal: recv_tt,
    };
    match translate_exit(inputs).await {
        ExitOutcome::Translated(DaemonEvent::PhaseComplete(PhaseCompletePayload {
            phase, ..
        })) => {
            assert_eq!(phase, PhaseName::OpenPr);
        }
        other => panic!("expected PhaseComplete, got {other:?}"),
    }

    // Ensure first handle's child is reaped.
    let _ = handle.child.wait().await;
}

#[tokio::test]
async fn unknown_terminal_subtype_is_classified_with_raw_subtype_preserved() {
    let (handle, _tmp) = spawn_phase_with_mode(
        "phase_unknown_subtype",
        PhaseName::Review,
        Mode::SpecDriven,
    )
    .await;

    let (_t, r) = oneshot::channel();
    let inputs = ExitTranslationInputs {
        child: handle.child,
        stream_rx: handle.stream_rx,
        phase: PhaseName::Review,
        stall_window: Duration::from_secs(10),
        tracker_terminal_signal: r,
    };
    match translate_exit(inputs).await {
        ExitOutcome::Translated(DaemonEvent::PhaseNonclean(payload)) => {
            assert_eq!(payload.classification, NoncleanClassification::UnknownSubtype);
            assert_eq!(
                payload.raw_subtype.as_deref(),
                Some("error_future_unknown_signal"),
                "raw_subtype must round-trip verbatim",
            );
        }
        other => panic!("expected PhaseNonclean(UnknownSubtype), got {other:?}"),
    }
}

#[tokio::test]
async fn nonzero_exit_without_result_event_classifies_as_non_zero() {
    let (handle, _tmp) =
        spawn_phase_with_mode("phase_nonzero_no_result", PhaseName::CiFix, Mode::SpecDriven)
            .await;
    let (_t, r) = oneshot::channel();
    let inputs = ExitTranslationInputs {
        child: handle.child,
        stream_rx: handle.stream_rx,
        phase: PhaseName::CiFix,
        stall_window: Duration::from_secs(10),
        tracker_terminal_signal: r,
    };
    match translate_exit(inputs).await {
        ExitOutcome::Translated(DaemonEvent::PhaseNonclean(payload)) => {
            assert_eq!(payload.classification, NoncleanClassification::NonZero);
            assert!(payload.raw_subtype.is_none());
        }
        other => panic!("expected PhaseNonclean(NonZero), got {other:?}"),
    }
}
