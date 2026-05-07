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
}
