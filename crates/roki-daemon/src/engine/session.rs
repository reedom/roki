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

use crate::capture::open_session_phase_files;
use crate::engine::outcome::PhaseKind;
use crate::engine::stall::Watchdog;
use crate::error::PhaseInfraError;

const SESSION_BETWEEN_TURN_STDERR_CAP: usize = 64 * 1024;

pub(crate) struct StderrBuf {
    bytes: Vec<u8>,
    truncated: bool,
}

pub(crate) struct OutEventsFiles {
    pub(crate) stdout: std::fs::File,
    pub(crate) events: std::fs::File,
}

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
    pub(crate) out_events: Arc<Mutex<Option<OutEventsFiles>>>,
    pub(crate) stderr_file: Arc<Mutex<Option<std::fs::File>>>,
    pub(crate) last_stderr_path: Mutex<Option<std::path::PathBuf>>,
    pub(crate) turn: watch::Sender<TurnState>,
    pub(crate) dir_rx: Mutex<mpsc::Receiver<SessionEvent>>,
    pub(crate) watchdog: Watchdog,
    pub(crate) default_stall_seconds: u32,
    pub(crate) between_turn_stderr: Arc<Mutex<StderrBuf>>,
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
        let out_events: Arc<Mutex<Option<OutEventsFiles>>> = Arc::new(Mutex::new(None));
        let stderr_file: Arc<Mutex<Option<std::fs::File>>> = Arc::new(Mutex::new(None));
        let between_turn_stderr = Arc::new(Mutex::new(StderrBuf {
            bytes: Vec::new(),
            truncated: false,
        }));
        let (turn_tx, turn_rx) = watch::channel(TurnState {
            kind: PhaseKind::Pre,
            generation: 0,
        });
        let (dir_tx, dir_rx) = mpsc::channel(8);

        // Stdout reader task takes out_events.
        {
            let watchdog = watchdog.clone();
            let oe = out_events.clone();
            let turn_rx = turn_rx.clone();
            tokio::spawn(reader_task(stdout, watchdog, oe, turn_rx, dir_tx));
        }
        // Stderr drain task takes stderr_file + buffer.
        {
            let sf = stderr_file.clone();
            let buf = between_turn_stderr.clone();
            tokio::spawn(stderr_drain_task(stderr, sf, buf));
        }

        Ok(Self {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            out_events,
            stderr_file,
            last_stderr_path: Mutex::new(None),
            turn: turn_tx,
            dir_rx: Mutex::new(dir_rx),
            watchdog,
            default_stall_seconds: cfg.default_stall_seconds,
            between_turn_stderr,
        })
    }

    pub async fn begin_turn(
        &self,
        iter_dir: &Path,
        kind: PhaseKind,
    ) -> Result<u64, PhaseInfraError> {
        use std::io::Write;

        let triple = open_session_phase_files(iter_dir, kind)?;
        let stderr_path = iter_dir.join(format!("{}.stderr", kind.as_str()));

        // Flush between-turn stderr buffer into the new turn's stderr file
        // before installing it into the slot.
        let mut stderr_file_handle = triple.stderr;
        {
            let mut buf_guard = self.between_turn_stderr.lock().await;
            if !buf_guard.bytes.is_empty() {
                let _ = stderr_file_handle.write_all(&buf_guard.bytes);
                buf_guard.bytes.clear();
                buf_guard.truncated = false;
            }
        }

        // Activate the new out_events slot (replacing the previous turn's).
        // Out_events lifetime spans from begin_turn to next begin_turn —
        // post-terminal advisory lines still land in this turn's files,
        // per fr:04 §72.
        {
            let mut oe_guard = self.out_events.lock().await;
            *oe_guard = Some(OutEventsFiles {
                stdout: triple.stdout,
                events: triple.events,
            });
        }

        // Activate the new stderr slot and remember its path for shutdown reopen.
        {
            let mut sf_guard = self.stderr_file.lock().await;
            *sf_guard = Some(stderr_file_handle);
        }
        {
            let mut last = self.last_stderr_path.lock().await;
            *last = Some(stderr_path);
        }

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
                // Close stderr slot for this turn — subsequent bytes go into
                // the between-turn buffer (flushed at next begin_turn).
                {
                    let mut sf = self.stderr_file.lock().await;
                    *sf = None;
                }
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
            Some(SessionEvent::SchemaDrift) => {
                // Close stderr slot for this turn — subsequent bytes go into
                // the between-turn buffer (flushed at next begin_turn).
                {
                    let mut sf = self.stderr_file.lock().await;
                    *sf = None;
                }
                Ok(PhaseOutcome::Failure {
                    kind: FailureKind::SchemaDrift,
                })
            }
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
        drop(child_guard);

        // Flush any remaining between-turn stderr bytes into the last-active
        // <phase>.stderr file. If the stderr_file slot is None (closed at
        // terminal), reopen the file at last_stderr_path in append mode.
        let mut buf_guard = self.between_turn_stderr.lock().await;
        if !buf_guard.bytes.is_empty() {
            use std::io::Write;
            let mut sf_guard = self.stderr_file.lock().await;
            if let Some(file) = sf_guard.as_mut() {
                let _ = file.write_all(&buf_guard.bytes);
            } else {
                let last_path = self.last_stderr_path.lock().await.clone();
                if let Some(path) = last_path {
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(&path)
                    {
                        let _ = file.write_all(&buf_guard.bytes);
                    }
                }
            }
            buf_guard.bytes.clear();
        }
    }
}

async fn reader_task(
    stdout: tokio::process::ChildStdout,
    watchdog: Watchdog,
    out_events: Arc<Mutex<Option<OutEventsFiles>>>,
    turn_rx: watch::Receiver<TurnState>,
    dir_tx: mpsc::Sender<SessionEvent>,
) {
    use crate::engine::stream::{scan_directive_line, DirectiveScan, LineSplitter};
    use std::io::Write;

    let mut splitter = LineSplitter::new(stdout);
    let mut last_emitted_generation: u64 = 0;

    loop {
        let line_res = splitter.next_line().await;
        watchdog.tick_stdout();
        match line_res {
            Ok(Some(line)) => {
                let state = turn_rx.borrow().clone();

                // Always tee to <phase>.stdout (and parseable lines to events.jsonl)
                // when an out_events slot is active. Out_events stays open from
                // begin_turn until the next begin_turn — post-terminal advisory
                // lines still land in this turn's files, per fr:04 §72.
                let scan = scan_directive_line(&line, state.kind);
                let parseable = !matches!(scan, DirectiveScan::NotJson);
                if let Some(handles) = out_events.lock().await.as_mut() {
                    let _ = handles.stdout.write_all(line.as_bytes());
                    let _ = handles.stdout.write_all(b"\n");
                    if parseable {
                        let _ = handles.events.write_all(line.as_bytes());
                        let _ = handles.events.write_all(b"\n");
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
    stderr_file: Arc<Mutex<Option<std::fs::File>>>,
    buf: Arc<Mutex<StderrBuf>>,
) {
    use std::io::Write;
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let mut sf_guard = stderr_file.lock().await;
                if let Some(file) = sf_guard.as_mut() {
                    let _ = file.write_all(&chunk[..n]);
                } else {
                    drop(sf_guard);
                    let mut buf_guard = buf.lock().await;
                    let remaining =
                        SESSION_BETWEEN_TURN_STDERR_CAP.saturating_sub(buf_guard.bytes.len());
                    let take = remaining.min(n);
                    buf_guard.bytes.extend_from_slice(&chunk[..take]);
                    if take < n && !buf_guard.truncated {
                        tracing::warn!(
                            target: "roki.engine.session",
                            cap = SESSION_BETWEEN_TURN_STDERR_CAP,
                            "phase_stderr_truncated"
                        );
                        buf_guard.truncated = true;
                    }
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

    #[tokio::test]
    async fn between_turn_stderr_flushes_into_next_turn() {
        // Fake AI:
        //   turn 1: emits "{ \"directive\": \"end\" }" then a stderr line.
        //   turn 2: emits another directive after stdin write.
        let script = r#"
emit_turn() {
  printf '{"directive":"end","tag":"%s"}\n' "$1"
  printf 'between-turn-line\n' >&2
}
read -r _line1
emit_turn t1
sleep 0.2
read -r _line2
emit_turn t2
"#;
        let cfg = SessionConfig {
            cli: "bash".to_string(),
            argv: vec!["bash".to_string(), "-c".to_string(), script.to_string()],
            default_stall_seconds: 5,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        };
        let sup = SessionSupervisor::spawn(cfg).await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let iter1 = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let _ = sup
            .run_turn(&iter1, PhaseKind::Post, b"go1\n", None)
            .await
            .unwrap();
        // Give the script time to write the between-turn stderr line.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let iter2 = create_iter_dir(tmp.path(), "X", Uuid::nil(), 2).unwrap();
        let _ = sup
            .run_turn(&iter2, PhaseKind::Post, b"go2\n", None)
            .await
            .unwrap();
        sup.shutdown(SessionShutdownReason::Completed).await;

        let iter2_stderr = std::fs::read_to_string(iter2.join("post.stderr")).unwrap();
        assert!(
            iter2_stderr.contains("between-turn-line"),
            "iter2/post.stderr should contain the bytes that arrived between turns: {iter2_stderr:?}"
        );
    }

    #[tokio::test]
    async fn post_terminal_advisory_lands_in_current_turn_events() {
        // Fake AI:
        //   turn 1: emit terminal directive, then an advisory parseable line
        //           after the directive, all on stdout. Then exit on stdin EOF.
        let script = r#"
read -r _line
printf '{"directive":"end","tag":"t1"}\n'
printf '{"type":"thinking","tag":"after-terminal"}\n'
"#;
        let cfg = SessionConfig {
            cli: "bash".to_string(),
            argv: vec!["bash".to_string(), "-c".to_string(), script.to_string()],
            default_stall_seconds: 5,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        };
        let sup = SessionSupervisor::spawn(cfg).await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let _ = sup
            .run_turn(&iter_dir, PhaseKind::Post, b"go\n", None)
            .await
            .unwrap();
        // Give the script time to write the after-terminal advisory line.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        sup.shutdown(SessionShutdownReason::Completed).await;

        let events = std::fs::read_to_string(iter_dir.join("post.events.jsonl")).unwrap();
        // Both terminal and post-terminal advisory should be in events.jsonl.
        assert!(
            events.contains("\"end\""),
            "events.jsonl should contain terminal directive: {events:?}"
        );
        assert!(
            events.contains("after-terminal"),
            "events.jsonl should contain post-terminal advisory line per fr:04 §72: {events:?}"
        );
    }
}
