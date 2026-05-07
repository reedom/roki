//! Long-lived session subprocess for slice-2 session-shape phases.
//!
//! `SessionSupervisor::spawn` constructs the child once per cycle. The
//! reader task drains stdout line-by-line and routes each line through:
//! - `events.jsonl` (parseable JSON only, per fr:04 §72)
//! - `<phase>.stdout` (every line, raw)
//! - directive channel (the first line whose `directive` field is legal
//!   for the active phase becomes the turn terminal)
//!
//! `run_turn` (Task 13) writes a rendered body to the child's stdin and
//! waits for the directive channel.
//!
//! `shutdown` (Task 14) closes stdin, waits the stall window, and SIGTERMs
//! / SIGKILLs as needed.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, watch, Mutex};

use crate::capture::{open_session_phase_files, SessionPhaseFiles};
use crate::engine::outcome::PhaseKind;
use crate::engine::stall::Watchdog;
use crate::error::PhaseInfraError;

/// Configuration for `SessionSupervisor::spawn`.
pub struct SessionConfig {
    pub cli: String,
    pub argv: Vec<String>,
    pub default_stall_seconds: u32,
    pub cwd: PathBuf,
    pub envs: Vec<(String, String)>,
}

/// One event the reader task pushes onto the directive channel.
#[derive(Debug)]
pub enum SessionEvent {
    Directive { value: Value },
    SchemaDrift,
    Exit,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnState {
    pub(crate) kind: PhaseKind,
    pub(crate) generation: u64,
}

pub struct SessionSupervisor {
    pub(crate) child: Mutex<Option<Child>>,
    pub(crate) stdin: Mutex<Option<ChildStdin>>,
    pub(crate) files: Arc<Mutex<Option<SessionPhaseFiles>>>,
    pub(crate) turn: watch::Sender<TurnState>,
    pub(crate) dir_rx: Mutex<mpsc::Receiver<SessionEvent>>,
    pub(crate) watchdog: Watchdog,
    pub(crate) default_stall_seconds: u32,
}

impl SessionSupervisor {
    pub async fn spawn(cfg: SessionConfig) -> Result<Self, PhaseInfraError> {
        use std::process::Stdio;
        use tokio::process::Command as TokioCommand;

        if cfg.argv.is_empty() {
            return Err(PhaseInfraError::SessionCliMissing);
        }

        let mut envs = std::collections::HashMap::new();
        for (k, v) in cfg.envs.iter() {
            envs.insert(k.clone(), v.clone());
        }

        let mut child = TokioCommand::new(&cfg.argv[0])
            .args(&cfg.argv[1..])
            .env_clear()
            .envs(envs)
            .current_dir(&cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| PhaseInfraError::SessionSpawn {
                cli: cfg.cli.clone(),
                source,
            })?;

        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let watchdog = Watchdog::new(cfg.default_stall_seconds);
        let files: Arc<Mutex<Option<SessionPhaseFiles>>> = Arc::new(Mutex::new(None));
        let (turn_tx, turn_rx) = watch::channel(TurnState {
            kind: PhaseKind::Pre,
            generation: 0,
        });
        let (dir_tx, dir_rx) = mpsc::channel(8);

        {
            let watchdog = watchdog.clone();
            let files = files.clone();
            let turn_rx = turn_rx.clone();
            tokio::spawn(reader_task(stdout, watchdog, files, turn_rx, dir_tx));
        }
        {
            let files = files.clone();
            tokio::spawn(stderr_drain_task(stderr, files));
        }

        Ok(Self {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            files,
            turn: turn_tx,
            dir_rx: Mutex::new(dir_rx),
            watchdog,
            default_stall_seconds: cfg.default_stall_seconds,
        })
    }

    pub async fn begin_turn(
        &self,
        iter_dir: &Path,
        kind: PhaseKind,
    ) -> Result<u64, PhaseInfraError> {
        let triple = open_session_phase_files(iter_dir, kind)?;
        let mut guard = self.files.lock().await;
        *guard = Some(triple);
        let new_state = {
            let prev = self.turn.borrow();
            TurnState {
                kind,
                generation: prev.generation + 1,
            }
        };
        let generation = new_state.generation;
        let _ = self.turn.send(new_state);
        Ok(generation)
    }

    /// Drive one turn end-to-end:
    ///   1. open the per-turn capture triple,
    ///   2. write `body_bytes` to the child's stdin (no close),
    ///   3. await a directive event from the reader task,
    ///   4. write `<phase>.response.json` and return `PhaseOutcome`.
    ///
    /// `stall_override` lets the cycle apply a `PhaseBody::Path::stall_seconds`
    /// override for the turn; the supervisor reverts to the default after.
    pub async fn run_turn(
        &self,
        iter_dir: &Path,
        kind: PhaseKind,
        body_bytes: &[u8],
        stall_override: Option<u32>,
    ) -> Result<crate::engine::outcome::PhaseOutcome, PhaseInfraError> {
        use crate::engine::outcome::{FailureKind, PhaseOutcome, PostDirective, PreDirective};
        use tokio::io::AsyncWriteExt;

        let _ = self.begin_turn(iter_dir, kind).await?;

        if let Some(seconds) = stall_override {
            self.watchdog.set_stall_seconds(seconds);
        } else {
            self.watchdog.set_stall_seconds(self.default_stall_seconds);
        }

        // Write body to stdin — keep stdin open across turns.
        {
            let mut stdin_guard = self.stdin.lock().await;
            let stdin = stdin_guard
                .as_mut()
                .ok_or(PhaseInfraError::SessionStdinClosed { phase: kind })?;
            stdin
                .write_all(body_bytes)
                .await
                .map_err(|_| PhaseInfraError::SessionStdinClosed { phase: kind })?;
            stdin
                .flush()
                .await
                .map_err(|_| PhaseInfraError::SessionStdinClosed { phase: kind })?;
        }

        // Await directive (or schema drift, or exit).
        let mut rx_guard = self.dir_rx.lock().await;
        let event = rx_guard.recv().await;

        match event {
            Some(SessionEvent::Directive { value }) => {
                crate::capture::write_response_json(iter_dir, kind, &value)?;
                let directive_str = value
                    .get("directive")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                match kind {
                    PhaseKind::Pre => {
                        let dir = PreDirective::try_from_str(directive_str)
                            .ok_or(PhaseInfraError::SessionStdoutClosed { phase: kind })?;
                        Ok(PhaseOutcome::PreDirective {
                            directive: dir,
                            payload: value,
                        })
                    }
                    PhaseKind::Post => {
                        let dir = PostDirective::try_from_str(directive_str)
                            .ok_or(PhaseInfraError::SessionStdoutClosed { phase: kind })?;
                        Ok(PhaseOutcome::PostDirective {
                            directive: dir,
                            payload: value,
                        })
                    }
                    PhaseKind::Run => Err(PhaseInfraError::ExecutorContract {
                        phase: kind,
                        got_variant: "PreDirective/PostDirective on Run",
                        iter: 0,
                    }),
                }
            }
            Some(SessionEvent::SchemaDrift) => Ok(PhaseOutcome::Failure {
                kind: FailureKind::SchemaDrift,
            }),
            Some(SessionEvent::Exit) => Ok(PhaseOutcome::Failure {
                kind: FailureKind::ProcessCrash,
            }),
            None => Err(PhaseInfraError::SessionStdoutClosed { phase: kind }),
        }
    }
}

/// Reason the cycle is asking the supervisor to wind down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionShutdownReason {
    /// Cycle ended via terminal directive — child should exit cleanly when
    /// stdin closes.
    Completed,
    /// `iter == max_iterations` and post returned `pre`/`run`. Per fr:01
    /// §123-125: close stdin, wait the stall window, SIGTERM if still alive.
    IterExhausted,
    /// Earlier failure on a phase. Child may be partially through a turn;
    /// terminate without waiting on stdin.
    Failed,
}

impl SessionSupervisor {
    pub async fn shutdown(&self, reason: SessionShutdownReason) {
        use std::time::Duration;
        use tokio::time::Instant;

        // Close stdin first (Completed / IterExhausted want a graceful exit).
        if !matches!(reason, SessionShutdownReason::Failed) {
            let mut stdin_guard = self.stdin.lock().await;
            *stdin_guard = None; // drop the writer, stdin EOFs
        }

        // Wait up to default_stall_seconds for the child to exit on its own.
        let deadline = Instant::now() + Duration::from_secs(self.default_stall_seconds as u64);
        loop {
            let mut child_guard = self.child.lock().await;
            let Some(child) = child_guard.as_mut() else {
                return;
            };
            if child.try_wait().ok().flatten().is_some() {
                *child_guard = None;
                return;
            }
            drop(child_guard);
            if Instant::now() >= deadline || matches!(reason, SessionShutdownReason::Failed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Stall window expired (or Failed reason) — SIGTERM, grace, SIGKILL.
        let mut child_guard = self.child.lock().await;
        let Some(child) = child_guard.as_mut() else {
            return;
        };
        crate::engine::stall::terminate_child_external(child).await;
        *child_guard = None;
    }
}

async fn reader_task(
    stdout: tokio::process::ChildStdout,
    watchdog: Watchdog,
    files: Arc<Mutex<Option<SessionPhaseFiles>>>,
    turn_rx: watch::Receiver<TurnState>,
    dir_tx: mpsc::Sender<SessionEvent>,
) {
    use std::io::Write;
    use crate::engine::stream::{scan_directive_line, DirectiveScan, LineSplitter};

    let mut splitter = LineSplitter::new(stdout);
    let mut last_emitted_generation: u64 = 0;

    loop {
        let line_res = splitter.next_line().await;
        watchdog.tick_stdout();
        match line_res {
            Ok(Some(line)) => {
                let state = turn_rx.borrow().clone();

                if let Some(triple) = files.lock().await.as_mut() {
                    let _ = triple.stdout.write_all(line.as_bytes());
                    let _ = triple.stdout.write_all(b"\n");
                }

                let scan = scan_directive_line(&line, state.kind);
                let parseable = !matches!(scan, DirectiveScan::NotJson);
                if parseable {
                    if let Some(triple) = files.lock().await.as_mut() {
                        let _ = triple.events.write_all(line.as_bytes());
                        let _ = triple.events.write_all(b"\n");
                    }
                }

                if state.generation > last_emitted_generation {
                    match scan {
                        DirectiveScan::PreTerminal { value, .. }
                        | DirectiveScan::PostTerminal { value, .. } => {
                            if dir_tx.send(SessionEvent::Directive { value }).await.is_err() {
                                break;
                            }
                            last_emitted_generation = state.generation;
                        }
                        DirectiveScan::SchemaDrift => {
                            if dir_tx.send(SessionEvent::SchemaDrift).await.is_err() {
                                break;
                            }
                            last_emitted_generation = state.generation;
                        }
                        _ => {}
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let _ = dir_tx.send(SessionEvent::Exit).await;
}

async fn stderr_drain_task(
    mut stderr: tokio::process::ChildStderr,
    files: Arc<Mutex<Option<SessionPhaseFiles>>>,
) {
    use std::io::Write;
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Some(triple) = files.lock().await.as_mut() {
                    let _ = triple.stderr.write_all(&buf[..n]);
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::create_iter_dir;
    use uuid::Uuid;

    fn echo_session_cfg() -> SessionConfig {
        SessionConfig {
            cli: "cat".to_string(),
            argv: vec!["cat".to_string()],
            default_stall_seconds: 5,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn spawn_creates_child_and_pipes() {
        let sup = SessionSupervisor::spawn(echo_session_cfg()).await.unwrap();
        let mut child_guard = sup.child.lock().await;
        let child = child_guard.as_mut().unwrap();
        assert!(child.try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn begin_turn_opens_three_files() {
        let sup = SessionSupervisor::spawn(echo_session_cfg()).await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir = create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 1).unwrap();
        let _gen = sup.begin_turn(&iter_dir, PhaseKind::Pre).await.unwrap();
        assert!(iter_dir.join("pre.stdout").is_file());
        assert!(iter_dir.join("pre.stderr").is_file());
        assert!(iter_dir.join("pre.events.jsonl").is_file());
    }

    /// Bash fake AI: reads stdin lines and emits a directive object on stdout
    /// per stdin line. Used to verify run_turn end-to-end.
    fn fake_session_cfg() -> SessionConfig {
        let script = r#"
while IFS= read -r line; do
  printf '{"type":"thinking"}\n'
  printf '{"directive":"end","echo":"%s"}\n' "$line"
done
"#;
        SessionConfig {
            cli: "bash".to_string(),
            argv: vec!["bash".to_string(), "-c".to_string(), script.to_string()],
            default_stall_seconds: 5,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn run_turn_returns_post_directive_end() {
        let sup = SessionSupervisor::spawn(fake_session_cfg()).await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir = create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 1).unwrap();
        let outcome = sup
            .run_turn(&iter_dir, PhaseKind::Post, b"hello\n", None)
            .await
            .unwrap();
        match outcome {
            crate::engine::outcome::PhaseOutcome::PostDirective { directive, payload } => {
                assert_eq!(directive, crate::engine::outcome::PostDirective::End);
                assert_eq!(payload.get("echo").and_then(|v| v.as_str()), Some("hello"));
            }
            other => panic!("expected PostDirective(End), got {other:?}"),
        }
        let events = std::fs::read_to_string(iter_dir.join("post.events.jsonl")).unwrap();
        assert!(events.contains("\"thinking\""));
        assert!(events.contains("\"end\""));
        assert!(iter_dir.join("post.response.json").is_file());
    }

    #[tokio::test]
    async fn shutdown_completed_closes_stdin_and_waits_for_clean_exit() {
        // The fake_session_cfg loop exits as soon as stdin closes, so a
        // Completed shutdown should observe a clean exit without SIGTERM.
        let sup = SessionSupervisor::spawn(fake_session_cfg()).await.unwrap();
        sup.shutdown(SessionShutdownReason::Completed).await;
        // Subsequent shutdown is a no-op.
        sup.shutdown(SessionShutdownReason::Completed).await;
    }

    #[tokio::test]
    async fn shutdown_iter_exhausted_terminates_after_stall_window() {
        // Use a child that ignores stdin close. Shutdown must SIGTERM after
        // the stall window (here 1 s).
        let cfg = SessionConfig {
            cli: "bash".to_string(),
            argv: vec![
                "bash".to_string(),
                "-c".to_string(),
                "trap '' TERM; sleep 30".to_string(),
            ],
            default_stall_seconds: 1,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        };
        let sup = SessionSupervisor::spawn(cfg).await.unwrap();
        let started = std::time::Instant::now();
        sup.shutdown(SessionShutdownReason::IterExhausted).await;
        // SIGTERM after 1 s stdin-close-wait + 5 s grace + SIGKILL — should finish well before 30 s.
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
    }
}
