//! Phase executor for command-shape phases.
//!
//! Resolves the ghq base path once per phase invocation, Liquid-renders argv
//! and stdin body, spawns the subprocess with stdout/stderr redirected into
//! the per-iter capture files, and translates the exit status + stdout
//! contents into a `PhaseOutcome` for the cycle driver.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::error::PhaseInfraError;

use super::context::{PhaseContext, roki_env_pairs};
use super::directive::{PostParse, PreParse, parse_post_directive, parse_pre_directive};
use super::outcome::{FailureKind, PhaseBody, PhaseKind, PhaseOutcome};
use super::template::render_str;

#[async_trait]
pub trait PhaseExecutor: Send + Sync {
    async fn execute(
        &self,
        kind: PhaseKind,
        body: &PhaseBody,
        ctx: &PhaseContext,
        iter_dir: &Path,
    ) -> Result<PhaseOutcome, PhaseInfraError>;
}

/// Stall window resolution at construction time. Lets the executor honour
/// either the shape default or a per-file override without re-reading
/// config in the hot path.
#[derive(Debug, Clone, Copy)]
pub enum StallWindow {
    /// Use this many seconds. Wired in by Task 17 once per-phase override
    /// flows through the executor.
    #[allow(dead_code)]
    Override(u32),
    /// Use the command-shape default from `[default.ai.command].stall_seconds`.
    CommandDefault(u32),
}

impl StallWindow {
    pub fn seconds(self) -> u32 {
        match self {
            StallWindow::Override(n) | StallWindow::CommandDefault(n) => n,
        }
    }
}

/// Production phase executor for command-shape phases.
pub struct CommandPhaseExecutor {
    /// `[default.ai.command].cli` from `roki.toml`. Used as the argv source
    /// for inline-prompt and path bodies that don't carry a `cli` override.
    pub default_cli: String,
    /// Resolved stall window applied to every command-shape phase. A
    /// per-file `stall_seconds` override on a `Path` body still wins inside
    /// `execute`; this field is the fallback when the body has no override.
    pub stall: StallWindow,
}

#[async_trait]
impl PhaseExecutor for CommandPhaseExecutor {
    async fn execute(
        &self,
        kind: PhaseKind,
        body: &PhaseBody,
        ctx: &PhaseContext,
        iter_dir: &Path,
    ) -> Result<PhaseOutcome, PhaseInfraError> {
        let cwd = resolve_ghq_base(&ctx.repo.ghq).await?;
        let resolved_stall = body
            .stall_seconds_override()
            .unwrap_or_else(|| self.stall.seconds());
        execute_at(
            &self.default_cli,
            kind,
            body,
            ctx,
            iter_dir,
            &cwd,
            resolved_stall,
        )
        .await
    }
}

/// Inner pipeline shared by production and unit tests. Takes a resolved
/// `cwd` so tests can bypass `ghq list -p` and exercise the full argv +
/// stdin + env + capture path against a tempdir. `stall_seconds` is the
/// already-resolved watchdog window (per-file override or shape default).
async fn execute_at(
    default_cli: &str,
    kind: PhaseKind,
    body: &PhaseBody,
    ctx: &PhaseContext,
    iter_dir: &Path,
    cwd: &Path,
    stall_seconds: u32,
) -> Result<PhaseOutcome, PhaseInfraError> {
    // 2. Build argv + stdin body.
    let (argv_template, stdin_template_opt) = match body {
        PhaseBody::InlineCmd { cmd } => {
            // sh -c <rendered>
            (format!("sh -c {}", shell_words::quote(cmd)), None)
        }
        PhaseBody::InlinePrompt { prompt } => (default_cli.to_string(), Some(prompt.clone())),
        PhaseBody::Path {
            path, cli_override, ..
        } => {
            // Read the workflow body from disk. The path was resolved at
            // config-load time against the workflow file's parent so the
            // executor reads the same file regardless of the daemon's
            // working directory. Frontmatter is stripped; anything after
            // a closing `---` (or the whole file if no frontmatter) is
            // the rendered body. cli_override wins over default_cli.
            let raw = match tokio::fs::read_to_string(path).await {
                Ok(s) => s,
                Err(source) => {
                    return Err(PhaseInfraError::WorkflowBodyUnreadable {
                        path: path.clone(),
                        source,
                    });
                }
            };
            let body_text = strip_frontmatter(&raw).to_string();
            let cli = cli_override
                .clone()
                .unwrap_or_else(|| default_cli.to_string());
            (cli, Some(body_text))
        }
    };

    // 3. Liquid render argv + stdin. Render failures are directive-level
    // failures routed via `FailureKind::TemplateError`; the underlying
    // Liquid error is captured in a `tracing::warn` so the operator log
    // identifies the failed expression and which stage (argv or stdin)
    // tripped the renderer.
    let argv_rendered = match render_str(&argv_template, ctx) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(
                phase = kind.as_str(),
                iter = ctx.cycle.iter,
                stage = "argv",
                template = %argv_template,
                error = %err,
                "phase template render failed"
            );
            return Ok(PhaseOutcome::Failure {
                kind: FailureKind::TemplateError,
            });
        }
    };
    let stdin_rendered = match stdin_template_opt {
        Some(t) => match render_str(&t, ctx) {
            Ok(s) => Some(s),
            Err(err) => {
                tracing::warn!(
                    phase = kind.as_str(),
                    iter = ctx.cycle.iter,
                    stage = "stdin",
                    error = %err,
                    "phase template render failed"
                );
                return Ok(PhaseOutcome::Failure {
                    kind: FailureKind::TemplateError,
                });
            }
        },
        None => None,
    };

    // 4. shell-words split argv.
    let argv = shell_words::split(&argv_rendered).map_err(|err| PhaseInfraError::Spawn {
        cmd: argv_rendered.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string()),
    })?;
    let Some((bin, rest)) = argv.split_first() else {
        return Err(PhaseInfraError::Spawn {
            cmd: argv_rendered,
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"),
        });
    };

    // 5. Open the on-disk stdout/stderr files via the existing helper.
    let (stdout_file, stderr_file) = crate::capture::open_phase_files(iter_dir, kind)?;

    // 6. Build the Command (piped stdio so we can tee).
    let env_pairs = roki_env_pairs(ctx);
    let mut cmd = Command::new(bin);
    cmd.args(rest)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_rendered.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    // env_clear so only ROKI_* + a small passthrough set is present.
    cmd.env_clear();
    for var in ["PATH", "HOME", "USER"] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    for (k, v) in env_pairs {
        cmd.env(k, v);
    }

    // 7. Spawn.
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|source| PhaseInfraError::Spawn {
        cmd: argv_rendered.clone(),
        source,
    })?;

    // 8. Write stdin once if needed; close it.
    if let Some(body) = stdin_rendered.as_ref() {
        // We set `Stdio::piped()` above whenever stdin_rendered is Some,
        // so child.stdin must be present here. If the handle is already
        // gone, the child would receive an empty stdin and the rendered
        // body would be silently dropped; surface that as an infra error
        // rather than letting the AI subprocess run without its prompt.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| PhaseInfraError::StdinUnavailable {
                cmd: argv_rendered.clone(),
            })?;
        stdin
            .write_all(body.as_bytes())
            .await
            .map_err(|source| PhaseInfraError::StdinWrite {
                cmd: argv_rendered.clone(),
                source,
            })?;
        drop(stdin);
    }

    // 9. Tee stdout and drain stderr via tokio tasks while the watchdog
    //    polls the child for stall. Per-byte `tick_stdout` keeps the
    //    watchdog honest; the stall surfaces as `FailureKind::Stall`.
    let stdout_pipe = child.stdout.take().expect("piped");
    let stderr_pipe = child.stderr.take().expect("piped");
    let watchdog = crate::engine::stall::Watchdog::new(stall_seconds);

    let stdout_task = {
        let wd = watchdog.clone();
        let raw = stdout_file;
        let kind_for_task = kind;
        let iter_dir_for_task = iter_dir.to_path_buf();
        tokio::spawn(async move {
            tee_stdout(stdout_pipe, raw, wd, kind_for_task, iter_dir_for_task).await;
        })
    };

    let stderr_task = {
        let raw = stderr_file;
        tokio::spawn(async move {
            drain_stderr(stderr_pipe, raw).await;
        })
    };

    // 10. Watchdog drives termination on stall. When it fires, the
    //     child has been signalled (SIGTERM, then SIGKILL after grace),
    //     so `child.wait()` will succeed with a signal-encoded status.
    let stall_outcome = watchdog.run(&mut child).await;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let exit_status = child.wait().await.map_err(|source| PhaseInfraError::Wait {
        cmd: argv_rendered.clone(),
        source,
    })?;
    let duration_seconds = started.elapsed().as_secs();

    if stall_outcome == crate::engine::stall::StallOutcome::StalledThenTerminated {
        return Ok(PhaseOutcome::Failure {
            kind: FailureKind::Stall,
        });
    }

    // 9. Translate exit + stdout into PhaseOutcome.
    match kind {
        PhaseKind::Run => {
            let exit_code = exit_status.code().unwrap_or(-1);
            crate::capture::write_run_exit_code(iter_dir, exit_code)?;
            Ok(PhaseOutcome::RunDone {
                exit_code,
                duration_seconds,
            })
        }
        PhaseKind::Pre => {
            let stdout_path = iter_dir.join(format!("{}.stdout", kind.as_str()));
            let bytes = std::fs::read(&stdout_path).map_err(|source| PhaseInfraError::Spawn {
                cmd: argv_rendered.clone(),
                source,
            })?;
            match parse_pre_directive(&bytes, exit_status.success()) {
                PreParse::Ok { directive, payload } => {
                    crate::capture::write_response_json(iter_dir, kind, &payload)?;
                    Ok(PhaseOutcome::PreDirective { directive, payload })
                }
                PreParse::Failed(kind) => Ok(PhaseOutcome::Failure { kind }),
            }
        }
        PhaseKind::Post => {
            let stdout_path = iter_dir.join(format!("{}.stdout", kind.as_str()));
            let bytes = std::fs::read(&stdout_path).map_err(|source| PhaseInfraError::Spawn {
                cmd: argv_rendered.clone(),
                source,
            })?;
            match parse_post_directive(&bytes, exit_status.success()) {
                PostParse::Ok { directive, payload } => {
                    crate::capture::write_response_json(iter_dir, kind, &payload)?;
                    Ok(PhaseOutcome::PostDirective { directive, payload })
                }
                PostParse::Failed(kind) => Ok(PhaseOutcome::Failure { kind }),
            }
        }
    }
}

/// Strip optional YAML frontmatter (`---` … `---` at file start) and return
/// the body. Returns the input unchanged when no frontmatter is present.
//
// Exposed to `engine::cycle` so the session-shape dispatch helper can render
// path-form bodies through the same frontmatter-stripping path as the
// command-shape executor.
pub(crate) fn strip_frontmatter(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return raw;
    }
    let after_open = match trimmed.strip_prefix("---") {
        Some(rest) => rest.trim_start_matches('\n'),
        None => return raw,
    };
    if let Some(close_idx) = after_open.find("\n---") {
        let after_close = &after_open[close_idx + 4..]; // skip "\n---"
        return after_close.trim_start_matches('\n');
    }
    raw
}

/// Resolve the absolute path of the operator's checkout via
/// `ghq list -p <ghq>`. Returns `RepoNotFound` when ghq has no entry.
//
// Exposed to `engine::cycle` so the cycle driver can resolve cwd once when
// constructing a `SessionSupervisor` for session-shape phases (the supervisor
// is built before any phase invocation, so cwd cannot come from the executor).
pub(crate) async fn resolve_ghq_base(ghq: &str) -> Result<std::path::PathBuf, PhaseInfraError> {
    // Test-support seam: if `ROKI_GHQ_BASE_OVERRIDE` is set, use it directly.
    // The release binary never reads this env var because the integration
    // test sets it per-spawn; production env never has it.
    if let Ok(override_path) = std::env::var("ROKI_GHQ_BASE_OVERRIDE") {
        if !override_path.is_empty() {
            return Ok(std::path::PathBuf::from(override_path));
        }
    }
    let out = Command::new("ghq")
        .arg("list")
        .arg("-p")
        .arg(ghq)
        .output()
        .await
        .map_err(|source| PhaseInfraError::Spawn {
            cmd: format!("ghq list -p {ghq}"),
            source,
        })?;
    if !out.status.success() {
        return Err(PhaseInfraError::RepoNotFound {
            ghq: ghq.to_string(),
        });
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PhaseInfraError::RepoNotFound {
            ghq: ghq.to_string(),
        })?;
    Ok(std::path::PathBuf::from(line))
}

/// Tee one process's stdout into the on-disk capture file while ticking the
/// watchdog on every read. During `Run`, every line is also fed to the
/// run-terminal scanner; the first claude/codex `result` event is
/// pretty-printed to `<iter>/run.terminal.json` mid-stream so downstream
/// readers don't have to wait for child exit. Pre/post phases skip the scan
/// because the slice-1 directive scan still consumes `<phase>.stdout` after
/// exit.
async fn tee_stdout(
    stdout_pipe: tokio::process::ChildStdout,
    mut raw_writer: std::fs::File,
    watchdog: crate::engine::stall::Watchdog,
    kind: crate::engine::outcome::PhaseKind,
    iter_dir: std::path::PathBuf,
) {
    use crate::engine::stream::{LineSplitter, scan_run_terminal_line};
    use std::io::Write;

    let mut splitter = LineSplitter::new(stdout_pipe);
    let mut terminal_written = false;

    loop {
        let line_res = splitter.next_line().await;
        watchdog.tick_stdout();
        match line_res {
            Ok(Some(line)) => {
                let _ = raw_writer.write_all(line.as_bytes());
                let _ = raw_writer.write_all(b"\n");
                if matches!(kind, crate::engine::outcome::PhaseKind::Run) && !terminal_written {
                    if let Some(value) = scan_run_terminal_line(&line) {
                        if let Err(err) = crate::capture::write_run_terminal_json(&iter_dir, &value)
                        {
                            tracing::warn!(target: "roki.engine.run_terminal", error = ?err, "run.terminal.json write failed");
                        } else {
                            terminal_written = true;
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

/// Drain stderr to the on-disk capture file. We don't tick the watchdog on
/// stderr — only stdout silence counts as a stall per fr:04.
async fn drain_stderr(mut stderr_pipe: tokio::process::ChildStderr, mut raw_writer: std::fs::File) {
    use std::io::Write;
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    loop {
        match stderr_pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let _ = raw_writer.write_all(&buf[..n]);
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::AdmittedTicket;
    use crate::config::roki::*;
    use crate::engine::outcome::{CycleKind, PreDirective};
    use crate::linear::ticket::NormalizedTicket;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn admitted() -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "ENG-9".to_string(),
                Some("u1".to_string()),
                "in_progress".to_string(),
                vec![],
                "T".to_string(),
                "B".to_string(),
            ),
            ghq: env!("CARGO_MANIFEST_DIR").to_string(),
        }
    }

    fn cfg() -> RokiConfig {
        RokiConfig {
            linear: LinearSection {
                token: "x".to_string(),
            },
            linear_webhook: LinearWebhookSection {
                bind: "127.0.0.1".to_string(),
                port: 8000,
                secret: None,
            },
            default_ai_command: DefaultAiCommandSection {
                cli: "echo".to_string(),
                stall_seconds: 300,
            },
            engine: EngineSection { max_iterations: 10 },
            paths: PathsSection {
                workflow: PathBuf::from("/tmp"),
                session_root: PathBuf::from("/tmp"),
            },
            log: LogSection::default(),
            default_ai_session: None,
        }
    }

    /// Test executor that bypasses ghq resolution and uses a caller-supplied cwd.
    struct DirectExec {
        default_cli: String,
        cwd: std::path::PathBuf,
    }

    #[async_trait]
    impl PhaseExecutor for DirectExec {
        async fn execute(
            &self,
            kind: PhaseKind,
            body: &PhaseBody,
            ctx: &PhaseContext,
            iter_dir: &Path,
        ) -> Result<PhaseOutcome, PhaseInfraError> {
            let argv_template = match body {
                PhaseBody::InlineCmd { cmd } => format!("sh -c {}", shell_words::quote(cmd)),
                _ => panic!("DirectExec only supports InlineCmd"),
            };
            let argv_rendered =
                render_str(&argv_template, ctx).map_err(|_| PhaseInfraError::Spawn {
                    cmd: argv_template.clone(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "render failed"),
                })?;
            let argv = shell_words::split(&argv_rendered).unwrap();
            let (bin, rest) = argv.split_first().unwrap();
            let (stdout_file, stderr_file) = crate::capture::open_phase_files(iter_dir, kind)?;
            let stdout_handle = stdout_file.try_clone().unwrap();
            let stderr_handle = stderr_file.try_clone().unwrap();
            let started = Instant::now();
            let mut child = Command::new(bin)
                .args(rest)
                .current_dir(&self.cwd)
                .stdout(Stdio::from(stdout_handle))
                .stderr(Stdio::from(stderr_handle))
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let exit_status = child.wait().await.unwrap();
            let duration_seconds = started.elapsed().as_secs();
            drop(stdout_file);
            drop(stderr_file);
            let _ = self.default_cli.len();

            match kind {
                PhaseKind::Run => {
                    let exit_code = exit_status.code().unwrap_or(-1);
                    crate::capture::write_run_exit_code(iter_dir, exit_code)?;
                    Ok(PhaseOutcome::RunDone {
                        exit_code,
                        duration_seconds,
                    })
                }
                PhaseKind::Pre => {
                    let bytes = std::fs::read(iter_dir.join("pre.stdout")).unwrap();
                    match parse_pre_directive(&bytes, exit_status.success()) {
                        PreParse::Ok { directive, payload } => {
                            crate::capture::write_response_json(iter_dir, kind, &payload)?;
                            Ok(PhaseOutcome::PreDirective { directive, payload })
                        }
                        PreParse::Failed(k) => Ok(PhaseOutcome::Failure { kind: k }),
                    }
                }
                PhaseKind::Post => {
                    let bytes = std::fs::read(iter_dir.join("post.stdout")).unwrap();
                    match parse_post_directive(&bytes, exit_status.success()) {
                        PostParse::Ok { directive, payload } => {
                            crate::capture::write_response_json(iter_dir, kind, &payload)?;
                            Ok(PhaseOutcome::PostDirective { directive, payload })
                        }
                        PostParse::Failed(k) => Ok(PhaseOutcome::Failure { kind: k }),
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn run_phase_writes_exit_code_and_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        ctx.set_iter(1);
        let body = PhaseBody::InlineCmd {
            cmd: "printf hello; printf err 1>&2; exit 5".into(),
        };

        let out = exec
            .execute(PhaseKind::Run, &body, &ctx, &iter_dir)
            .await
            .unwrap();
        match out {
            PhaseOutcome::RunDone { exit_code, .. } => assert_eq!(exit_code, 5),
            other => panic!("unexpected outcome: {other:?}"),
        }
        let exit_text = std::fs::read_to_string(iter_dir.join("run.exit_code")).unwrap();
        assert_eq!(exit_text.trim(), "5");
        let stdout_bytes = std::fs::read_to_string(iter_dir.join("run.stdout")).unwrap();
        assert!(stdout_bytes.contains("hello"));
        let stderr_bytes = std::fs::read_to_string(iter_dir.join("run.stderr")).unwrap();
        assert!(stderr_bytes.contains("err"));
    }

    #[tokio::test]
    async fn pre_phase_parses_directive_and_writes_response_json() {
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        let body = PhaseBody::InlineCmd {
            cmd: r#"printf '{"directive":"run","outcome":"ok"}'"#.to_string(),
        };

        let out = exec
            .execute(PhaseKind::Pre, &body, &ctx, &iter_dir)
            .await
            .unwrap();
        match out {
            PhaseOutcome::PreDirective { directive, payload } => {
                assert_eq!(directive, PreDirective::Run);
                assert_eq!(payload["outcome"], "ok");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let resp_path = iter_dir.join("pre.response.json");
        let resp = std::fs::read_to_string(&resp_path).unwrap();
        assert!(resp.contains("\"directive\""));
    }

    #[tokio::test]
    async fn pre_phase_unparseable_yields_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        let body = PhaseBody::InlineCmd {
            cmd: r#"printf 'not json'"#.to_string(),
        };

        let out = exec
            .execute(PhaseKind::Pre, &body, &ctx, &iter_dir)
            .await
            .unwrap();
        match out {
            PhaseOutcome::Failure {
                kind: FailureKind::Unparseable,
            } => {}
            other => panic!("expected Unparseable failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_phase_propagates_roki_env_vars_to_child() {
        // FR contract: ROKI_TICKET_ID, ROKI_REPO_GHQ, ROKI_CYCLE_* arrive
        // intact in the child process. Use the production execute_at path so
        // the env_clear + roki_env_pairs + Command::env loop are exercised.
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        ctx.set_iter(1);
        let body = PhaseBody::InlineCmd {
            cmd: r#"printf 'TID=%s|GHQ=%s|ITER=%s' "$ROKI_TICKET_ID" "$ROKI_REPO" "$ROKI_CYCLE_ITER""#
                .to_string(),
        };
        let cwd = tmp.path().to_path_buf();

        let out = super::execute_at("echo", PhaseKind::Run, &body, &ctx, &iter_dir, &cwd, 300)
            .await
            .unwrap();
        match out {
            PhaseOutcome::RunDone { exit_code, .. } => assert_eq!(exit_code, 0),
            other => panic!("unexpected outcome: {other:?}"),
        }
        let stdout = std::fs::read_to_string(iter_dir.join("run.stdout")).unwrap();
        assert!(stdout.contains("TID=ENG-9"), "stdout={stdout}");
        assert!(
            stdout.contains(&format!("GHQ={}", env!("CARGO_MANIFEST_DIR"))),
            "stdout={stdout}"
        );
        assert!(stdout.contains("ITER=1"), "stdout={stdout}");
    }

    #[tokio::test]
    async fn pre_phase_inline_prompt_feeds_stdin_to_default_cli() {
        // InlinePrompt: argv comes from default_cli, the rendered prompt is
        // written to the child's stdin. Use `cat` as default_cli so stdout
        // mirrors stdin and we can assert the directive JSON parses through.
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        let body = PhaseBody::InlinePrompt {
            prompt: r#"{"directive":"run","outcome":"piped"}"#.to_string(),
        };
        let cwd = tmp.path().to_path_buf();

        let out = super::execute_at("cat", PhaseKind::Pre, &body, &ctx, &iter_dir, &cwd, 300)
            .await
            .unwrap();
        match out {
            PhaseOutcome::PreDirective { directive, payload } => {
                assert_eq!(directive, PreDirective::Run);
                assert_eq!(payload["outcome"], "piped");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let resp = std::fs::read_to_string(iter_dir.join("pre.response.json")).unwrap();
        assert!(resp.contains("\"directive\""));
    }

    #[tokio::test]
    async fn pre_phase_path_body_reads_file_strips_frontmatter_and_honors_cli_override() {
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let workflow_body = "---\ncli: ignored-by-frontmatter-not-implemented\n---\n{\"directive\":\"end\",\"outcome\":\"path-ok\"}";
        let body_path = tmp.path().join("phase.md");
        std::fs::write(&body_path, workflow_body).unwrap();

        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        let body = PhaseBody::Path {
            path: body_path.clone(),
            cli_override: Some("cat".to_string()),
            shape: crate::engine::outcome::PhaseShape::Session,
            stall_seconds: None,
        };
        let cwd = tmp.path().to_path_buf();

        // default_cli intentionally bogus to assert cli_override wins.
        let out = super::execute_at(
            "/bin/no-such-cli",
            PhaseKind::Pre,
            &body,
            &ctx,
            &iter_dir,
            &cwd,
            300,
        )
        .await
        .unwrap();
        match out {
            PhaseOutcome::PreDirective { directive, payload } => {
                assert_eq!(directive, PreDirective::End);
                assert_eq!(payload["outcome"], "path-ok");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn path_body_missing_file_returns_workflow_body_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let missing = tmp.path().join("does-not-exist.md");
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        let body = PhaseBody::Path {
            path: missing.clone(),
            cli_override: Some("cat".to_string()),
            shape: crate::engine::outcome::PhaseShape::Session,
            stall_seconds: None,
        };
        let cwd = tmp.path().to_path_buf();

        let err = super::execute_at("cat", PhaseKind::Pre, &body, &ctx, &iter_dir, &cwd, 300)
            .await
            .expect_err("missing workflow body must surface infra error");
        match err {
            PhaseInfraError::WorkflowBodyUnreadable { path: p, .. } => assert_eq!(p, missing),
            other => panic!("expected WorkflowBodyUnreadable, got {other:?}"),
        }
    }

    #[test]
    fn strip_frontmatter_returns_body_after_yaml() {
        let raw = "---\nfoo: bar\n---\nbody-line\nmore\n";
        assert_eq!(super::strip_frontmatter(raw), "body-line\nmore\n");
    }

    #[test]
    fn strip_frontmatter_passthrough_when_absent() {
        let raw = "no frontmatter here";
        assert_eq!(super::strip_frontmatter(raw), raw);
    }

    #[tokio::test]
    async fn run_phase_extracts_terminal_result_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-1", uuid::Uuid::nil(), 1).unwrap();
        let body = crate::engine::outcome::PhaseBody::InlineCmd {
            cmd: r#"printf '%s\n' '{"type":"thinking"}' '{"type":"result","is_error":false,"result":"ok"}'"#
                .to_string(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        let cwd = tmp.path().to_path_buf();
        let outcome = super::execute_at(
            "echo unused",
            crate::engine::outcome::PhaseKind::Run,
            &body,
            &ctx,
            &iter_dir,
            &cwd,
            30,
        )
        .await
        .unwrap();
        match outcome {
            crate::engine::outcome::PhaseOutcome::RunDone { exit_code, .. } => {
                assert_eq!(exit_code, 0);
            }
            other => panic!("expected RunDone, got {other:?}"),
        }
        let body_text = std::fs::read_to_string(iter_dir.join("run.terminal.json")).unwrap();
        assert!(body_text.contains("\"is_error\""));
        assert!(body_text.contains("\"result\""));
    }

    #[tokio::test]
    async fn run_phase_omits_terminal_when_no_result_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-1", uuid::Uuid::nil(), 1).unwrap();
        let body = crate::engine::outcome::PhaseBody::InlineCmd {
            cmd: "echo plain".to_string(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        let cwd = tmp.path().to_path_buf();
        let _ = super::execute_at(
            "echo unused",
            crate::engine::outcome::PhaseKind::Run,
            &body,
            &ctx,
            &iter_dir,
            &cwd,
            30,
        )
        .await
        .unwrap();
        assert!(!iter_dir.join("run.terminal.json").exists());
    }

    #[tokio::test]
    async fn command_phase_stalls_on_idle_child() {
        // Idle stdout for longer than the stall window must surface as
        // FailureKind::Stall via the watchdog-driven termination path.
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir =
            crate::capture::create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 1).unwrap();
        let body = PhaseBody::InlineCmd {
            cmd: "sleep 30".to_string(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(), CycleKind::Rule);
        let cwd = tmp.path().to_path_buf();
        let outcome = super::execute_at(
            "echo unused",
            PhaseKind::Run,
            &body,
            &ctx,
            &iter_dir,
            &cwd,
            1, // 1-second stall window
        )
        .await
        .unwrap();
        match outcome {
            PhaseOutcome::Failure {
                kind: FailureKind::Stall,
            } => {}
            other => panic!("expected Failure(Stall), got {other:?}"),
        }
    }
}
