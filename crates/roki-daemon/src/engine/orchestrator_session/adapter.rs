//! Long-lived orchestrator session adapter: spawns one
//! `claude --input-format stream-json --output-format stream-json` process
//! per ticket, drains a [`mpsc`] channel of [`DaemonEvent`]s onto its
//! stdin, parses turn-aligned [`OrchestratorAction`] objects off its stdout,
//! and surfaces parser/process events to the orchestrator core via a single
//! [`mpsc::Receiver<ActionEvent>`].
//!
//! Wire convention (delivery of the rendered system prompt):
//! - The system prompt is delivered as the FIRST line written to claude's
//!   stdin, encoded as a stream-json `system`/`init` envelope:
//!   `{"type":"system","subtype":"init","system_prompt":"..."}` with the
//!   prompt body in the `system_prompt` field. The fake_claude harness
//!   used for unit tests reads exactly this first line and echoes a
//!   correlation marker on stdout so the test can assert delivery.
//! - This was selected over `--system-prompt-file <path>` because the
//!   stdin-first variant is cleanly representable in the existing
//!   `fake_claude` harness and survives CWD changes without requiring an
//!   absolute path resolved by the operator.
//!
//! Phase / event ordering is the orchestrator core's responsibility: this
//! adapter writes events strictly in the order it receives them off the
//! `stdin_tx` channel. `tracker_terminal` events are not given any inherent
//! priority here; callers must order them ahead of phase events when the
//! ticket has reached a tracker-terminal state.
//!
//! Spec refs: requirements.md Req 4.1, 4.2, 5.1, 5.2, 6.6, 9.6, 11.5, 11.8.

use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::engine::claude::{ClaudeBinary, ClaudeError};
use crate::engine::orchestrator_session::action_parser::{
    ActionParser, OrchestratorAction, ParseTurnOutcome,
};
use crate::engine::orchestrator_session::events::{DaemonEvent, serialize_one_per_line};
use crate::logging::{PerIssueDebugSink, RoleTag, StreamTag};
use crate::orchestrator::state::{IssueId, Mode};
use crate::permissions::{PermissionStrategy, ResolvedPermission, Sandbox};

/// Channel buffer for daemon -> orchestrator stdin. The orchestrator core
/// emits at most a handful of events per turn; 32 is comfortably above the
/// observed bound.
const STDIN_CHANNEL_CAPACITY: usize = 32;

/// Channel buffer for adapter -> caller action stream. Sized to absorb a
/// burst of actions during shutdown without backpressuring the parser task.
const ACTION_CHANNEL_CAPACITY: usize = 32;

/// Bounded grace window for stdin-close-driven graceful exit before the
/// shutdown helper escalates to `Child::kill`. Falls back to this when the
/// caller does not pass an explicit window.
const DEFAULT_STALL_GRACE: Duration = Duration::from_secs(5);

/// Inputs the caller assembles per ticket. All policy resolution
/// (permissions, allowed_tools, system prompt, debug sink) happens upstream
/// so the adapter focuses on the spawn + IO loop only.
#[derive(Debug)]
pub struct OrchestratorLaunchContext {
    pub issue: IssueId,
    pub mode: Mode,
    /// Per-session scratch directory used as CWD and as the parent of the
    /// rendered `--settings` JSON file.
    pub session_tempdir: PathBuf,
    /// Rendered Liquid orchestrator system prompt body.
    pub system_prompt: String,
    /// Canonical orchestrator allowlist (`extension.orchestrator.allowed_tools`).
    pub allowed_tools: Vec<String>,
    /// Optional per-issue debug sink. When `Some`, stdout and stderr lines
    /// are appended verbatim to the issue's debug log file (Req 11.5).
    pub debug_sink: Option<PerIssueDebugSink>,
}

/// Adapter constructor inputs. Permissions are pre-resolved by the caller
/// via [`crate::permissions::PermissionResolver::resolve_for_orchestrator`].
#[derive(Debug)]
pub struct OrchestratorSessionAdapter {
    binary: ClaudeBinary,
    permissions: ResolvedPermission,
}

impl OrchestratorSessionAdapter {
    /// Build a new adapter. Panics in debug builds if the resolved
    /// permission descriptor does not match the documented orchestrator pin
    /// (read-only sandbox + reject elicitations + allowlist-bound).
    pub fn new(binary: ClaudeBinary, permissions: ResolvedPermission) -> Self {
        debug_assert_eq!(permissions.sandbox, Sandbox::ReadOnly);
        debug_assert!(permissions.reject_elicitations);
        debug_assert!(matches!(
            permissions.strategy,
            PermissionStrategy::SettingsAllowlist { .. }
        ));
        Self { binary, permissions }
    }

    /// Spawn the orchestrator session for one ticket. The returned handle
    /// owns the child plus three IO tasks (stdin writer, stdout parser,
    /// stderr drainer); call [`OrchestratorSessionHandle::shutdown`] to
    /// close the session cleanly.
    pub async fn launch(
        &self,
        ctx: OrchestratorLaunchContext,
    ) -> Result<OrchestratorSessionHandle, AdapterError> {
        let OrchestratorLaunchContext {
            issue,
            mode: _,
            session_tempdir,
            system_prompt,
            allowed_tools,
            debug_sink,
        } = ctx;

        let settings_path = session_tempdir.join("orchestrator-settings.json");
        write_settings_file(&settings_path, &allowed_tools, &self.permissions)?;

        let mut process = self
            .binary
            .clone()
            .spawn_builder()
            .with_settings(settings_path)
            .args([
                "--input-format".to_owned(),
                "stream-json".to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
            ])
            .cwd(session_tempdir)
            .spawn()
            .await
            .map_err(AdapterError::Spawn)?;

        // Deliver the rendered system prompt as the first stdin line so the
        // orchestrator session sees it before any DaemonEvent.
        let init_line = serialize_init_envelope(&system_prompt)?;
        process
            .stdin
            .write_all(init_line.as_bytes())
            .await
            .map_err(|source| AdapterError::WriteSystemPrompt { source })?;
        process
            .stdin
            .flush()
            .await
            .map_err(|source| AdapterError::WriteSystemPrompt { source })?;

        let (stdin_tx, stdin_rx) = mpsc::channel::<DaemonEvent>(STDIN_CHANNEL_CAPACITY);
        let (action_tx, action_rx) = mpsc::channel::<ActionEvent>(ACTION_CHANNEL_CAPACITY);

        let crate::engine::claude::ClaudeProcess {
            child,
            stdin,
            stdout,
            stderr,
        } = process;

        let stdin_task = tokio::spawn(stdin_writer_task(stdin, stdin_rx));

        // The debug sink is moved into the stdout task; the stderr task
        // shares it via a parking_lot-style mutex would be ideal, but to
        // avoid pulling in deps we split: stdout owns the sink for stdout
        // lines; stderr lines fall back to `tracing::warn!` only.
        // Per Req 11.5 the per-issue file should still capture stderr; we
        // achieve this by interleaving via a single tokio::sync::Mutex.
        let debug = debug_sink.map(|sink| std::sync::Arc::new(tokio::sync::Mutex::new(sink)));
        let stdout_task = tokio::spawn(stdout_parser_task(
            stdout,
            action_tx.clone(),
            debug.clone(),
        ));
        let stderr_task = tokio::spawn(stderr_drainer_task(
            stderr,
            issue.clone(),
            action_tx.clone(),
            debug,
        ));

        Ok(OrchestratorSessionHandle {
            stdin_tx,
            action_rx,
            child,
            stdin_task: Some(stdin_task),
            stdout_task: Some(stdout_task),
            stderr_task: Some(stderr_task),
            action_tx,
        })
    }
}

/// Errors surfaced from [`OrchestratorSessionAdapter::launch`].
#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("failed to spawn orchestrator session: {0}")]
    Spawn(#[source] ClaudeError),

    #[error("failed to write rendered settings JSON: {source}")]
    WriteSettings {
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write system prompt to orchestrator stdin: {source}")]
    WriteSystemPrompt {
        #[source]
        source: std::io::Error,
    },

    #[error("internal serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Owned handle over a spawned orchestrator session.
#[derive(Debug)]
pub struct OrchestratorSessionHandle {
    /// Send daemon events to the orchestrator's stdin. Dropping the sender
    /// (or calling [`Self::shutdown`]) closes stdin, which the orchestrator
    /// reads as end-of-input and exits gracefully.
    pub stdin_tx: mpsc::Sender<DaemonEvent>,
    /// Stream of parser/process events. Closes when the child exits and
    /// the parser task drains.
    pub action_rx: mpsc::Receiver<ActionEvent>,
    child: Child,
    stdin_task: Option<JoinHandle<()>>,
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    /// Held so internal IO tasks can keep emitting after shutdown closes
    /// the public sender. Currently unused beyond keeping the channel open;
    /// future work may use this to inject synthetic events on shutdown.
    #[allow(dead_code)]
    action_tx: mpsc::Sender<ActionEvent>,
}

impl OrchestratorSessionHandle {
    /// Close the orchestrator session.
    ///
    /// 1. Drops the stdin sender so the writer task drains and closes stdin.
    /// 2. Awaits child exit within `grace`; on timeout, escalates to `kill`.
    /// 3. Joins the IO tasks and returns the child's [`ExitStatus`].
    pub async fn shutdown(mut self, grace: Option<Duration>) -> ExitStatus {
        // Drop the public stdin sender so the writer task observes
        // channel close and shuts stdin cleanly.
        drop(self.stdin_tx);

        let grace = grace.unwrap_or(DEFAULT_STALL_GRACE);
        let status = match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => {
                warn!(role = "orchestrator", error = %err, "child wait failed; killing");
                let _ = self.child.kill().await;
                self.child.wait().await.unwrap_or_else(|_| failed_status())
            }
            Err(_elapsed) => {
                warn!(
                    role = "orchestrator",
                    "graceful exit window elapsed; sending kill"
                );
                let _ = self.child.kill().await;
                self.child.wait().await.unwrap_or_else(|_| failed_status())
            }
        };

        for handle in [
            self.stdin_task.take(),
            self.stdout_task.take(),
            self.stderr_task.take(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = handle.await;
        }

        status
    }
}

/// Adapter -> caller event stream surface. One [`ActionEvent`] is emitted
/// per parser turn or per-process lifecycle observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionEvent {
    /// Parser produced a typed action.
    Action(OrchestratorAction),
    /// First-time drift; carries the schema-reminder reprompt body.
    Drift { reprompt: String },
    /// Second consecutive drift; the caller is expected to terminate the
    /// session and route to `Inactive(orchestrator_unparseable)`.
    TerminalDrift { raw_stdout: String },
    /// The child exited. `raw_stdout` is the last non-empty stdout line we
    /// observed (best-effort) for inclusion in the structured log.
    ProcessExit { status: ExitStatus, raw_stdout: String },
}

// ---------------------------------------------------------------------------
// IO tasks
// ---------------------------------------------------------------------------

async fn stdin_writer_task(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::Receiver<DaemonEvent>,
) {
    while let Some(event) = rx.recv().await {
        let line = match serialize_one_per_line(&event) {
            Ok(line) => line,
            Err(err) => {
                warn!(role = "orchestrator", error = %err, "failed to serialize daemon event");
                continue;
            }
        };
        if let Err(err) = stdin.write_all(line.as_bytes()).await {
            warn!(role = "orchestrator", error = %err, "stdin write failed; closing");
            return;
        }
        if let Err(err) = stdin.flush().await {
            warn!(role = "orchestrator", error = %err, "stdin flush failed; closing");
            return;
        }
    }
    // Channel closed: drop stdin so claude sees EOF and exits.
    let _ = stdin.shutdown().await;
}

type SharedDebugSink = std::sync::Arc<tokio::sync::Mutex<PerIssueDebugSink>>;

async fn stdout_parser_task(
    mut stdout: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    action_tx: mpsc::Sender<ActionEvent>,
    debug: Option<SharedDebugSink>,
) {
    let mut parser = ActionParser::new();
    let mut buffer: Vec<String> = Vec::new();
    let mut last_raw: String = String::new();

    loop {
        match stdout.next_line().await {
            Ok(Some(line)) => {
                if !line.trim().is_empty() {
                    last_raw = line.clone();
                }
                if let Some(sink) = &debug {
                    sink.lock()
                        .await
                        .append(StreamTag::Stdout, &RoleTag::Orchestrator, &line);
                }
                buffer.push(line);
                // Heuristic per task spec: every line carrying a complete
                // JSON object resembling an OrchestratorAction (has an
                // `"action":` field) closes a turn.
                if line_resembles_action(buffer.last().map(String::as_str).unwrap_or("")) {
                    let outcome = parser.parse_turn(&buffer);
                    buffer.clear();
                    match emit_outcome(&action_tx, outcome).await {
                        Ok(false) => return,
                        Ok(true) => {}
                        Err(()) => return,
                    }
                }
            }
            Ok(None) => {
                // Stdout closed; flush whatever's buffered as a final turn.
                if !buffer.is_empty() {
                    let outcome = parser.parse_turn(&buffer);
                    buffer.clear();
                    let _ = emit_outcome(&action_tx, outcome).await;
                }
                let _ = action_tx
                    .send(ActionEvent::ProcessExit {
                        status: failed_status(),
                        raw_stdout: last_raw,
                    })
                    .await;
                return;
            }
            Err(err) => {
                warn!(role = "orchestrator", error = %err, "stdout read error; ending parser");
                return;
            }
        }
    }
}

async fn stderr_drainer_task(
    mut stderr: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStderr>>,
    issue: IssueId,
    _action_tx: mpsc::Sender<ActionEvent>,
    debug: Option<SharedDebugSink>,
) {
    while let Ok(Some(line)) = stderr.next_line().await {
        warn!(
            role = "orchestrator",
            issue = %issue,
            stderr = %line,
            "orchestrator stderr"
        );
        if let Some(sink) = &debug {
            sink.lock()
                .await
                .append(StreamTag::Stderr, &RoleTag::Orchestrator, &line);
        }
    }
}

async fn emit_outcome(
    tx: &mpsc::Sender<ActionEvent>,
    outcome: ParseTurnOutcome,
) -> Result<bool, ()> {
    let event = match outcome {
        ParseTurnOutcome::Action(action) => ActionEvent::Action(action),
        ParseTurnOutcome::Drift { reprompt_payload } => ActionEvent::Drift {
            reprompt: reprompt_payload,
        },
        ParseTurnOutcome::TerminalDrift { last_raw_stdout } => ActionEvent::TerminalDrift {
            raw_stdout: last_raw_stdout,
        },
    };
    match tx.send(event).await {
        Ok(()) => Ok(true),
        // Receiver dropped: caller hung up. Stop emitting.
        Err(_) => Err(()),
    }
}

fn line_resembles_action(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return false;
    }
    // Cheap structural check: must parse as a JSON object with an `action`
    // key. This deliberately also matches drift turns whose JSON object
    // happens to carry `action` even with an unknown verb — the parser is
    // the authority, this gate just decides where a turn boundary lands.
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return false,
    };
    value
        .as_object()
        .map(|o| o.contains_key("action"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Settings rendering + system prompt envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OrchestratorSettings<'a> {
    permissions: SettingsPermissions<'a>,
    sandbox: &'static str,
    reject_elicitations: bool,
}

#[derive(Debug, Serialize)]
struct SettingsPermissions<'a> {
    allow: &'a [String],
}

fn write_settings_file(
    path: &std::path::Path,
    allowed_tools: &[String],
    permissions: &ResolvedPermission,
) -> Result<(), AdapterError> {
    let settings = OrchestratorSettings {
        permissions: SettingsPermissions {
            allow: allowed_tools,
        },
        // Daemon-pinned: orchestrator session is always read-only with
        // elicitations rejected, regardless of operator overrides.
        sandbox: "read-only",
        reject_elicitations: permissions.reject_elicitations,
    };
    let body = serde_json::to_vec_pretty(&settings).map_err(AdapterError::Serialize)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|source| AdapterError::WriteSettings { source })?;
    }
    std::fs::write(path, body).map_err(|source| AdapterError::WriteSettings { source })
}

#[derive(Debug, Serialize)]
struct SystemInitEnvelope<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    subtype: &'static str,
    system_prompt: &'a str,
}

fn serialize_init_envelope(system_prompt: &str) -> Result<String, AdapterError> {
    let env = SystemInitEnvelope {
        kind: "system",
        subtype: "init",
        system_prompt,
    };
    let mut buf = serde_json::to_string(&env).map_err(AdapterError::Serialize)?;
    buf.push('\n');
    Ok(buf)
}

#[cfg(unix)]
fn failed_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(1 << 8)
}

#[cfg(not(unix))]
fn failed_status() -> ExitStatus {
    // Non-unix is out of scope; return whatever default the platform allows.
    ExitStatus::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionResolver;
    use std::path::Path;
    use std::sync::OnceLock;
    use tracing_test::traced_test;

    #[cfg(unix)]
    fn fake_claude_path() -> &'static Path {
        static PATH: OnceLock<PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            // Build the example binary on demand so the test does not
            // require an out-of-band cargo invocation. We reuse the same
            // workspace target dir to share artifacts.
            let status = std::process::Command::new(env!("CARGO"))
                .args(["build", "--quiet", "--example", "fake_claude", "-p", "roki-daemon"])
                .status()
                .expect("invoke cargo build --example fake_claude");
            assert!(status.success(), "fake_claude example build failed");
            // The binary lands under `target/debug/examples/fake_claude`.
            // CARGO_MANIFEST_DIR points at crates/roki-daemon, so step up
            // two levels to reach the workspace root before joining.
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

    #[cfg(unix)]
    fn make_adapter(_tmp: &Path) -> OrchestratorSessionAdapter {
        let binary = ClaudeBinary::discover(Some(fake_claude_path()))
            .expect("fake_claude discoverable");
        let permissions = PermissionResolver::resolve_for_orchestrator(&[
            "Read".to_owned(),
            "mcp__linear*".to_owned(),
        ]);
        OrchestratorSessionAdapter::new(binary, permissions)
    }

    #[cfg(unix)]
    fn write_mode(dir: &Path, mode: &str) {
        std::fs::write(dir.join(".fake_claude_mode"), mode).unwrap();
    }

    #[cfg(unix)]
    fn launch_ctx(tempdir: PathBuf) -> OrchestratorLaunchContext {
        OrchestratorLaunchContext {
            issue: IssueId::from("ENG-1"),
            mode: Mode::SpecDriven,
            session_tempdir: tempdir,
            system_prompt: "ROKI-SYSTEM-PROMPT-MARKER".to_owned(),
            allowed_tools: vec!["Read".to_owned()],
            debug_sink: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_emits_first_action_with_system_prompt_delivered() {
        let tmp = tempfile::tempdir().unwrap();
        write_mode(tmp.path(), "single_action");

        let adapter = make_adapter(tmp.path());
        let ctx = launch_ctx(tmp.path().to_path_buf());
        let mut handle = adapter.launch(ctx).await.expect("adapter launch");

        let event = tokio::time::timeout(Duration::from_secs(5), handle.action_rx.recv())
            .await
            .expect("recv timeout")
            .expect("action event");
        match event {
            ActionEvent::Action(action) => {
                assert_eq!(
                    action.action,
                    crate::engine::orchestrator_session::action_parser::ActionKind::RunPhase
                );
                assert_eq!(
                    action.phase,
                    Some(crate::engine::orchestrator_session::action_parser::PhaseName::Implement)
                );
                // The fake binary echoes a marker proving the system prompt
                // was the first stdin line it observed.
                assert!(action.reason.as_str().contains("ROKI-SYSTEM-PROMPT-MARKER"));
            }
            other => panic!("expected Action, got {other:?}"),
        }

        let _status = handle.shutdown(Some(Duration::from_secs(3))).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn second_action_after_phase_complete_event_is_delivered() {
        use crate::engine::orchestrator_session::events::{
            DaemonEvent, PhaseCompletePayload,
        };
        use crate::engine::phase_subprocess::catalog::PhaseName;

        let tmp = tempfile::tempdir().unwrap();
        write_mode(tmp.path(), "echo_phase_complete");

        let adapter = make_adapter(tmp.path());
        let ctx = launch_ctx(tmp.path().to_path_buf());
        let mut handle = adapter.launch(ctx).await.expect("adapter launch");

        // Wait for the first action.
        let first = tokio::time::timeout(Duration::from_secs(5), handle.action_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first, ActionEvent::Action(_)));

        // Send a phase_complete event; the fake responds with a follow-up
        // action that nominates `open_pr`.
        handle
            .stdin_tx
            .send(DaemonEvent::PhaseComplete(PhaseCompletePayload {
                phase: PhaseName::Implement,
                result: serde_json::json!({"subtype":"success"}),
                pr_url: None,
                review_artifact_path: None,
                classify: None,
            }))
            .await
            .expect("send phase_complete");

        let second = tokio::time::timeout(Duration::from_secs(5), handle.action_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match second {
            ActionEvent::Action(action) => {
                assert_eq!(action.phase, Some(PhaseName::OpenPr));
            }
            other => panic!("expected open_pr Action, got {other:?}"),
        }

        let _status = handle.shutdown(Some(Duration::from_secs(3))).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_closes_stdin_and_exits_within_grace() {
        let tmp = tempfile::tempdir().unwrap();
        write_mode(tmp.path(), "wait_for_stdin_close");

        let adapter = make_adapter(tmp.path());
        let ctx = launch_ctx(tmp.path().to_path_buf());
        let handle = adapter.launch(ctx).await.expect("adapter launch");

        let started = std::time::Instant::now();
        let status = handle.shutdown(Some(Duration::from_secs(3))).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "shutdown did not complete inside grace ({elapsed:?})"
        );
        assert!(status.success(), "fake_claude should exit cleanly on EOF");
    }

    #[cfg(unix)]
    #[traced_test]
    #[tokio::test]
    async fn stderr_lines_emit_warn_tagged_orchestrator_role() {
        let tmp = tempfile::tempdir().unwrap();
        write_mode(tmp.path(), "stderr_then_action");

        let adapter = make_adapter(tmp.path());
        let ctx = launch_ctx(tmp.path().to_path_buf());
        let mut handle = adapter.launch(ctx).await.expect("adapter launch");

        // Drain at least one action so we are sure stderr has been pumped
        // through too (the fake interleaves them).
        let _ = tokio::time::timeout(Duration::from_secs(5), handle.action_rx.recv())
            .await
            .unwrap();
        let _status = handle.shutdown(Some(Duration::from_secs(3))).await;

        assert!(logs_contain("orchestrator stderr"));
        assert!(logs_contain("ROKI-STDERR-MARKER"));
    }
}
