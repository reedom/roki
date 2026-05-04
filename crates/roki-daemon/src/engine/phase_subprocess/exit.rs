//! Phase subprocess exit translation + per-phase stall detector.
//!
//! Reads the terminal `result` event off the stream-json receiver, classifies
//! the exit per [Req 5.7, 5.8, 5.9, 11.5, 11.8], and yields a typed
//! [`DaemonEvent`] (or the `TrackerTerminal` solo event when the SIGTERM was
//! tracker-driven). Emits a single `phase.completed` structured log event on
//! every exit.
//!
//! Spec refs: requirements.md Req 5.7, 5.8, 5.9, 11.5, 11.8;
//! design.md "PhaseSubprocessAdapter" exit translation flow.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::warn;

use crate::engine::orchestrator_session::events::{
    ClassifyOutcome, ClassifyPath, DaemonEvent, NoncleanClassification, PhaseCompletePayload,
    PhaseNoncleanPayload, TrackerTerminalPayload,
};
use crate::engine::phase_subprocess::catalog::PhaseName;
use crate::engine::stream::{StreamLine, TerminalSubtype, classify_terminal};

/// Result of running [`translate_exit`] over a single phase subprocess.
///
/// `TrackerTerminalSolo` is the documented exception for tracker-driven
/// SIGTERMs (Req 5.8): the adapter does NOT translate the exit into
/// `phase_complete` / `phase_nonclean`; instead the caller's pre-built
/// `tracker_terminal` payload is forwarded solo.
#[derive(Debug, Clone, PartialEq)]
pub enum ExitOutcome {
    /// Translated into a `phase_complete` / `phase_nonclean` event suitable
    /// for the orchestrator's stdin.
    Translated(DaemonEvent),
    /// Tracker-terminal SIGTERM: adapter discards the phase exit (captured
    /// in the structured log only) and surfaces the tracker-terminal event
    /// solo per Req 5.8.
    TrackerTerminalSolo(DaemonEvent),
}

/// Inputs the caller assembles per phase exit.
pub struct ExitTranslationInputs {
    pub child: Child,
    pub stream_rx: mpsc::Receiver<StreamLine>,
    pub phase: PhaseName,
    pub stall_window: Duration,
    /// Resolves to a pre-built `TrackerTerminal` payload when the SIGTERM
    /// was caused by a tracker-terminal observation; the adapter then
    /// returns the tracker-terminal event solo without translating the
    /// exit.
    pub tracker_terminal_signal: oneshot::Receiver<TrackerTerminalPayload>,
}

/// Drive the phase subprocess to completion: read the terminal `result`
/// event off `stream_rx`, watch for per-phase stall, and translate the
/// exit into a typed [`DaemonEvent`].
///
/// Behavior contract (Req 5.7-5.9, 5.8 exception, 11.8 logging):
/// - On a `Result(success)` line: emit `PhaseComplete` with parsed
///   per-phase fields (PR url for `open_pr`, review artifact for
///   `finalize_review`, classify path for `classify`).
/// - On a `Result(<documented non-success>)` or `Result(<unknown>)` line:
///   emit `PhaseNonclean` with the classification and verbatim
///   `raw_subtype`.
/// - On `--max-turns` exhaustion (`Result(error_max_turns)`): emit
///   `PhaseNonclean(MaxTurnsExhausted)`.
/// - On non-zero exit / signal / stall SIGTERM with no terminal result:
///   emit the matching `PhaseNonclean` variant.
/// - On tracker-terminal observation: SIGTERM the child if still alive,
///   discard the phase exit, and return the tracker-terminal payload solo.
/// - Always emits one structured `phase.completed` log event.
pub async fn translate_exit(inputs: ExitTranslationInputs) -> ExitOutcome {
    let ExitTranslationInputs {
        mut child,
        mut stream_rx,
        phase,
        stall_window,
        mut tracker_terminal_signal,
    } = inputs;

    let issue_id = "<phase-issue>"; // caller logs richer context via spans

    let started_at = Instant::now();
    let mut last_result: Option<(String, Value)> = None;
    let mut stall_deadline = Instant::now() + stall_window;

    let outcome = loop {
        // Re-arm the stall sleep against the current deadline so concurrent
        // stream activity extends the window rather than racing the timer.
        let now = Instant::now();
        let until_stall = stall_deadline.saturating_duration_since(now);
        let stall_sleep = tokio::time::sleep(until_stall);
        tokio::pin!(stall_sleep);

        tokio::select! {
            biased;

            // Tracker-terminal observation always wins: SIGTERM the child,
            // discard the phase exit, and return the tracker-terminal solo.
            tracker = &mut tracker_terminal_signal => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                emit_phase_completed_log(
                    issue_id, phase, started_at,
                    Some("tracker_terminal_override"),
                );
                let payload = match tracker {
                    Ok(p) => p,
                    Err(_) => {
                        // Sender dropped without firing: degrade to a
                        // non-zero phase exit so the caller still gets a
                        // typed event.
                        return ExitOutcome::Translated(DaemonEvent::PhaseNonclean(
                            PhaseNoncleanPayload {
                                phase,
                                classification: NoncleanClassification::NonZero,
                                raw_subtype: None,
                                additional_context: None,
                            },
                        ));
                    }
                };
                return ExitOutcome::TrackerTerminalSolo(DaemonEvent::TrackerTerminal(payload));
            }

            line = stream_rx.recv() => {
                match line {
                    Some(StreamLine::Result { subtype, payload }) => {
                        last_result = Some((subtype, payload));
                        // The terminal result is observed; let the child
                        // wind down and route translation through the
                        // `child.wait()` branch below.
                        stall_deadline = Instant::now() + stall_window;
                    }
                    Some(_other) => {
                        // Any non-terminal stream activity counts as activity
                        // for the per-phase stall window.
                        stall_deadline = Instant::now() + stall_window;
                    }
                    None => {
                        // Stdout closed; await child wait.
                        break translate_after_eof(&mut child, last_result.take(), phase).await;
                    }
                }
            }

            _ = &mut stall_sleep => {
                // SIGTERM the child and emit the stall classification.
                let _ = child.start_kill();
                let _ = child.wait().await;
                break DaemonEvent::PhaseNonclean(PhaseNoncleanPayload {
                    phase,
                    classification: NoncleanClassification::Stall,
                    raw_subtype: None,
                    additional_context: None,
                });
            }
        }
    };

    let parsed = parse_outcome_label(&outcome);
    emit_phase_completed_log(issue_id, phase, started_at, parsed);

    ExitOutcome::Translated(outcome)
}

/// After stdout EOF: await the child, then translate the captured terminal
/// `result` (if any) plus the exit status into a typed `DaemonEvent`.
async fn translate_after_eof(
    child: &mut Child,
    last_result: Option<(String, Value)>,
    phase: PhaseName,
) -> DaemonEvent {
    let status = match child.wait().await {
        Ok(s) => s,
        Err(_) => {
            return DaemonEvent::PhaseNonclean(PhaseNoncleanPayload {
                phase,
                classification: NoncleanClassification::NonZero,
                raw_subtype: None,
                additional_context: None,
            });
        }
    };

    if let Some((subtype, payload)) = last_result {
        return classify_result_event(phase, subtype, payload);
    }

    // No terminal `result` event observed.
    classify_status_only(phase, status)
}

fn classify_result_event(phase: PhaseName, subtype: String, payload: Value) -> DaemonEvent {
    match classify_terminal(&subtype) {
        TerminalSubtype::Success => DaemonEvent::PhaseComplete(build_phase_complete(
            phase, subtype, payload,
        )),
        TerminalSubtype::ErrorMaxTurns => DaemonEvent::PhaseNonclean(PhaseNoncleanPayload {
            phase,
            classification: NoncleanClassification::MaxTurnsExhausted,
            raw_subtype: Some(subtype),
            additional_context: None,
        }),
        TerminalSubtype::ErrorDuringExecution
        | TerminalSubtype::NonSuccessKnown(_) => DaemonEvent::PhaseNonclean(
            PhaseNoncleanPayload {
                phase,
                classification: NoncleanClassification::NonSuccessSubtype,
                raw_subtype: Some(subtype),
                additional_context: None,
            },
        ),
        TerminalSubtype::Unknown(raw) => DaemonEvent::PhaseNonclean(PhaseNoncleanPayload {
            phase,
            classification: NoncleanClassification::UnknownSubtype,
            raw_subtype: Some(raw),
            additional_context: None,
        }),
    }
}

fn classify_status_only(phase: PhaseName, status: std::process::ExitStatus) -> DaemonEvent {
    let classification = if !status.success() {
        // Distinguish signal-killed vs. non-zero exit on Unix; non-Unix
        // platforms collapse both to NonZero (out-of-scope per design.md).
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if status.signal().is_some() {
                NoncleanClassification::Signal
            } else {
                NoncleanClassification::NonZero
            }
        }
        #[cfg(not(unix))]
        {
            NoncleanClassification::NonZero
        }
    } else {
        // Clean exit but no terminal `result` event: treat as a non-zero
        // shape because the documented success path REQUIRES the result
        // event.
        NoncleanClassification::NonZero
    };
    DaemonEvent::PhaseNonclean(PhaseNoncleanPayload {
        phase,
        classification,
        raw_subtype: None,
        additional_context: None,
    })
}

fn build_phase_complete(
    phase: PhaseName,
    _subtype: String,
    payload: Value,
) -> PhaseCompletePayload {
    let pr_url = if matches!(phase, PhaseName::OpenPr) {
        payload
            .get("pr_url")
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else {
        None
    };

    let review_artifact_path = if matches!(phase, PhaseName::FinalizeReview) {
        payload
            .get("review_artifact_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
    } else {
        None
    };

    let classify = if matches!(phase, PhaseName::Classify) {
        parse_classify_outcome(&payload)
    } else {
        None
    };

    PhaseCompletePayload {
        phase,
        result: payload,
        pr_url,
        review_artifact_path,
        classify,
    }
}

fn parse_classify_outcome(payload: &Value) -> Option<ClassifyOutcome> {
    let path_str = payload.get("path").and_then(Value::as_str)?;
    let path = match path_str {
        "a" | "A" => ClassifyPath::A,
        "b" | "B" => ClassifyPath::B,
        "c" | "C" => ClassifyPath::C,
        "d" | "D" => ClassifyPath::D,
        "e" | "E" => ClassifyPath::E,
        _ => return None,
    };
    Some(ClassifyOutcome {
        path,
        suggested_command: payload
            .get("suggested_command")
            .and_then(Value::as_str)
            .map(str::to_owned),
        suggested_label: payload
            .get("suggested_label")
            .and_then(Value::as_str)
            .map(str::to_owned),
        target_feature: payload
            .get("target_feature")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn parse_outcome_label(event: &DaemonEvent) -> Option<&'static str> {
    match event {
        DaemonEvent::PhaseComplete(_) => Some("success"),
        DaemonEvent::PhaseNonclean(p) => Some(match p.classification {
            NoncleanClassification::NonZero => "non_zero",
            NoncleanClassification::Signal => "signal",
            NoncleanClassification::Stall => "stall",
            NoncleanClassification::MaxTurnsExhausted => "max_turns_exhausted",
            NoncleanClassification::NonSuccessSubtype => "non_success_subtype",
            NoncleanClassification::UnknownSubtype => "unknown_subtype",
        }),
        _ => None,
    }
}

fn emit_phase_completed_log(
    issue_id: &str,
    phase: PhaseName,
    started_at: Instant,
    parsed: Option<&str>,
) {
    let duration_ms = started_at.elapsed().as_millis() as u64;
    let role = format!("phase:{}", phase_key(phase));
    let outcome = parsed.unwrap_or("unparseable");
    // Single structured log event per phase exit per Req 11.8.
    tracing::info!(
        event = "phase.completed",
        role = %role,
        issue = %issue_id,
        duration_ms = duration_ms,
        outcome = %outcome,
        "phase subprocess exit translated",
    );
    if parsed.is_none() {
        warn!(role = %role, "phase exit could not be parsed cleanly");
    }
}

fn phase_key(phase: PhaseName) -> &'static str {
    match phase {
        PhaseName::Classify => "classify",
        PhaseName::Implement => "implement",
        PhaseName::Review => "review",
        PhaseName::Validate => "validate",
        PhaseName::OpenPr => "open_pr",
        PhaseName::CiFix => "ci_fix",
        PhaseName::FinalizeReview => "finalize_review",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::orchestrator_session::events::TrackerTerminalState;
    use crate::engine::phase_subprocess::adapter::PhaseSubprocessAdapter;
    use crate::engine::phase_subprocess::catalog::{
        PhaseLaunchContext, WorkflowPolicyHandle,
    };
    use crate::engine::stream::StreamLine;
    use crate::orchestrator::state::{IssueId, Mode};
    use crate::permissions::{PermissionResolver, PermissionStrategy};
    use crate::workflow::schema::{OrchestratorConfig, WorkflowPolicy};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tracing_test::traced_test;

    #[cfg(unix)]
    fn fake_claude_path() -> std::path::PathBuf {
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
    }

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

    #[cfg(unix)]
    async fn spawn_phase_with_mode(
        mode: &str,
        phase: PhaseName,
        ticket_mode: Mode,
    ) -> (
        crate::engine::phase_subprocess::adapter::PhaseProcessHandle,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".fake_claude_mode"), mode).unwrap();
        std::fs::write(tmp.path().join("settings.json"), b"{}").unwrap();

        let bin = crate::engine::claude::ClaudeBinary::discover(Some(&fake_claude_path()))
            .unwrap();
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

    #[cfg(unix)]
    #[traced_test]
    #[tokio::test]
    async fn success_translates_to_phase_complete_with_log_event() {
        let (handle, _tmp) = spawn_phase_with_mode(
            "phase_success",
            PhaseName::OpenPr,
            Mode::SpecDriven,
        )
        .await;
        let (_send_tt, recv_tt) = oneshot::channel();
        let inputs = ExitTranslationInputs {
            child: handle.child,
            stream_rx: handle.stream_rx,
            phase: PhaseName::OpenPr,
            stall_window: Duration::from_secs(10),
            tracker_terminal_signal: recv_tt,
        };
        match translate_exit(inputs).await {
            ExitOutcome::Translated(DaemonEvent::PhaseComplete(p)) => {
                assert_eq!(p.phase, PhaseName::OpenPr);
            }
            other => panic!("expected PhaseComplete, got {other:?}"),
        }
        assert!(logs_contain("phase.completed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn error_max_turns_translates_to_max_turns_exhausted() {
        let (handle, _tmp) = spawn_phase_with_mode(
            "phase_error_max_turns",
            PhaseName::Implement,
            Mode::SpecDriven,
        )
        .await;
        let (_t, r) = oneshot::channel();
        let inputs = ExitTranslationInputs {
            child: handle.child,
            stream_rx: handle.stream_rx,
            phase: PhaseName::Implement,
            stall_window: Duration::from_secs(10),
            tracker_terminal_signal: r,
        };
        match translate_exit(inputs).await {
            ExitOutcome::Translated(DaemonEvent::PhaseNonclean(p)) => {
                assert_eq!(p.classification, NoncleanClassification::MaxTurnsExhausted);
                assert_eq!(p.raw_subtype.as_deref(), Some("error_max_turns"));
            }
            other => panic!("expected PhaseNonclean(MaxTurnsExhausted), got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn error_during_execution_translates_to_non_success_subtype() {
        let (handle, _tmp) = spawn_phase_with_mode(
            "phase_error_during_execution",
            PhaseName::Implement,
            Mode::SpecDriven,
        )
        .await;
        let (_t, r) = oneshot::channel();
        let inputs = ExitTranslationInputs {
            child: handle.child,
            stream_rx: handle.stream_rx,
            phase: PhaseName::Implement,
            stall_window: Duration::from_secs(10),
            tracker_terminal_signal: r,
        };
        match translate_exit(inputs).await {
            ExitOutcome::Translated(DaemonEvent::PhaseNonclean(p)) => {
                assert_eq!(p.classification, NoncleanClassification::NonSuccessSubtype);
                assert_eq!(p.raw_subtype.as_deref(), Some("error_during_execution"));
            }
            other => panic!("expected PhaseNonclean(NonSuccessSubtype), got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unknown_subtype_forwards_raw_string_verbatim() {
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
            ExitOutcome::Translated(DaemonEvent::PhaseNonclean(p)) => {
                assert_eq!(p.classification, NoncleanClassification::UnknownSubtype);
                assert_eq!(
                    p.raw_subtype.as_deref(),
                    Some("error_future_unknown_signal")
                );
            }
            other => panic!("expected PhaseNonclean(UnknownSubtype), got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_without_result_event_is_non_zero_classification() {
        let (handle, _tmp) = spawn_phase_with_mode(
            "phase_nonzero_no_result",
            PhaseName::CiFix,
            Mode::SpecDriven,
        )
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
            ExitOutcome::Translated(DaemonEvent::PhaseNonclean(p)) => {
                assert_eq!(p.classification, NoncleanClassification::NonZero);
                assert!(p.raw_subtype.is_none());
            }
            other => panic!("expected PhaseNonclean(NonZero), got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn per_phase_stall_sigterms_and_emits_phase_nonclean_stall() {
        let (handle, _tmp) = spawn_phase_with_mode(
            "phase_stall",
            PhaseName::Implement,
            Mode::SpecDriven,
        )
        .await;
        let (_t, r) = oneshot::channel();
        let inputs = ExitTranslationInputs {
            child: handle.child,
            stream_rx: handle.stream_rx,
            phase: PhaseName::Implement,
            // Tight stall window so the test runs fast.
            stall_window: Duration::from_millis(120),
            tracker_terminal_signal: r,
        };
        let started = Instant::now();
        match translate_exit(inputs).await {
            ExitOutcome::Translated(DaemonEvent::PhaseNonclean(p)) => {
                assert_eq!(p.classification, NoncleanClassification::Stall);
            }
            other => panic!("expected PhaseNonclean(Stall), got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "stall path took too long ({:?})",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tracker_terminal_signal_returns_solo_event_without_translation() {
        let (handle, _tmp) = spawn_phase_with_mode(
            "phase_stall", // child sleeps, so stall+tracker race with biased-select
            PhaseName::Implement,
            Mode::SpecDriven,
        )
        .await;
        let (send_tt, recv_tt) = oneshot::channel();
        // Fire the tracker-terminal signal before driving the exit.
        let payload = TrackerTerminalPayload {
            terminal_state: TrackerTerminalState::Canceled,
            correlation_id: "corr-track".to_owned(),
            timestamp: time::OffsetDateTime::now_utc(),
        };
        send_tt.send(payload.clone()).unwrap();

        let inputs = ExitTranslationInputs {
            child: handle.child,
            stream_rx: handle.stream_rx,
            phase: PhaseName::Implement,
            stall_window: Duration::from_secs(60),
            tracker_terminal_signal: recv_tt,
        };
        match translate_exit(inputs).await {
            ExitOutcome::TrackerTerminalSolo(DaemonEvent::TrackerTerminal(p)) => {
                assert_eq!(p.terminal_state, TrackerTerminalState::Canceled);
                assert_eq!(p.correlation_id, "corr-track");
            }
            other => panic!("expected TrackerTerminalSolo, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classify_result_payload_carries_path_into_phase_complete() {
        // Pure unit test of `classify_result_event` so we cover the
        // classify-specific PhaseComplete envelope without spawning.
        let payload = json!({
            "subtype": "success",
            "path": "B",
            "suggested_command": "/kiro-spec-init",
            "suggested_label": "roki:impl",
            "target_feature": "foo",
        });
        let event = classify_result_event(PhaseName::Classify, "success".to_owned(), payload);
        match event {
            DaemonEvent::PhaseComplete(p) => {
                assert_eq!(p.phase, PhaseName::Classify);
                let classify = p.classify.expect("classify outcome populated");
                assert_eq!(classify.path, ClassifyPath::B);
                assert_eq!(classify.suggested_command.as_deref(), Some("/kiro-spec-init"));
                assert_eq!(classify.suggested_label.as_deref(), Some("roki:impl"));
                assert_eq!(classify.target_feature.as_deref(), Some("foo"));
            }
            other => panic!("expected PhaseComplete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_pr_result_payload_carries_pr_url_into_phase_complete() {
        let payload = json!({
            "subtype": "success",
            "pr_url": "https://github.com/x/y/pull/1",
        });
        let event = classify_result_event(PhaseName::OpenPr, "success".to_owned(), payload);
        match event {
            DaemonEvent::PhaseComplete(p) => {
                assert_eq!(p.pr_url.as_deref(), Some("https://github.com/x/y/pull/1"));
            }
            other => panic!("expected PhaseComplete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn finalize_review_result_payload_carries_review_artifact_path() {
        let payload = json!({
            "subtype": "success",
            "review_artifact_path": ".kiro/specs/foo/review.md",
        });
        let event =
            classify_result_event(PhaseName::FinalizeReview, "success".to_owned(), payload);
        match event {
            DaemonEvent::PhaseComplete(p) => {
                assert_eq!(
                    p.review_artifact_path.as_deref(),
                    Some(std::path::Path::new(".kiro/specs/foo/review.md"))
                );
            }
            other => panic!("expected PhaseComplete, got {other:?}"),
        }
    }

    fn _ensure_stream_line_visible() {
        // Compile-time assertion that the StreamLine import is reachable;
        // the symbol is re-exported via the integration tests.
        let _ = std::mem::size_of::<StreamLine>();
    }
}
