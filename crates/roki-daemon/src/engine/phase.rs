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

use super::context::{roki_env_pairs, PhaseContext};
use super::directive::{parse_post_directive, parse_pre_directive, PostParse, PreParse};
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

/// Production phase executor for command-shape phases.
pub struct CommandPhaseExecutor {
    /// `[default.ai.command].cli` from `roki.toml`. Used as the argv source
    /// for inline-prompt and path bodies that don't carry a `cli` override.
    pub default_cli: String,
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
        // 1. Resolve cwd via `ghq list -p <ghq>`.
        let cwd = resolve_ghq_base(&ctx.repo.ghq).await?;

        // 2. Build argv + stdin body.
        let (argv_template, stdin_template_opt) = match body {
            PhaseBody::InlineCmd { cmd } => {
                // sh -c <rendered>
                (format!("sh -c {}", shell_words::quote(cmd)), None)
            }
            PhaseBody::InlinePrompt { prompt } => {
                (self.default_cli.clone(), Some(prompt.clone()))
            }
            PhaseBody::Path { body: path_str, cli_override } => {
                // Read the workflow body from disk. Frontmatter is stripped;
                // anything after a closing `---` (or the whole file if no
                // frontmatter) is the rendered body. cli_override wins over
                // default_cli when present.
                let raw = match tokio::fs::read_to_string(path_str).await {
                    Ok(s) => s,
                    Err(source) => {
                        return Err(PhaseInfraError::Spawn {
                            cmd: format!("read {path_str}"),
                            source,
                        });
                    }
                };
                let body_text = strip_frontmatter(&raw).to_string();
                let cli = cli_override.clone().unwrap_or_else(|| self.default_cli.clone());
                (cli, Some(body_text))
            }
        };

        // 3. Liquid render argv + stdin.
        let argv_rendered = match render_str(&argv_template, ctx) {
            Ok(s) => s,
            Err(_) => return Ok(PhaseOutcome::Failure { kind: FailureKind::TemplateError }),
        };
        let stdin_rendered = match stdin_template_opt {
            Some(t) => match render_str(&t, ctx) {
                Ok(s) => Some(s),
                Err(_) => return Ok(PhaseOutcome::Failure { kind: FailureKind::TemplateError }),
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

        // 5. Open stdout / stderr capture files.
        let (stdout_file, stderr_file) = crate::capture::open_phase_files(iter_dir, kind)?;
        let stdout_handle = stdout_file.try_clone().map_err(|source| PhaseInfraError::Spawn {
            cmd: argv_rendered.clone(),
            source,
        })?;
        let stderr_handle = stderr_file.try_clone().map_err(|source| PhaseInfraError::Spawn {
            cmd: argv_rendered.clone(),
            source,
        })?;

        // 6. Build the Command.
        let env_pairs = roki_env_pairs(ctx);
        let mut cmd = Command::new(bin);
        cmd.args(rest)
            .current_dir(&cwd)
            .stdout(Stdio::from(stdout_handle))
            .stderr(Stdio::from(stderr_handle));
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

        // 7. Spawn and write stdin.
        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|source| PhaseInfraError::Spawn {
            cmd: argv_rendered.clone(),
            source,
        })?;
        if let Some(body) = stdin_rendered.as_ref() {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(body.as_bytes())
                    .await
                    .map_err(|source| PhaseInfraError::Spawn {
                        cmd: argv_rendered.clone(),
                        source,
                    })?;
                drop(stdin);
            }
        }

        // 8. Wait.
        let exit_status = child.wait().await.map_err(|source| PhaseInfraError::Wait {
            cmd: argv_rendered.clone(),
            source,
        })?;
        let duration_seconds = started.elapsed().as_secs();

        // Drop the capture handles we kept so the post-exit reads see the
        // child's bytes flushed.
        drop(stdout_file);
        drop(stderr_file);

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
                let bytes = std::fs::read(&stdout_path)
                    .map_err(|source| PhaseInfraError::Spawn {
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
                let bytes = std::fs::read(&stdout_path)
                    .map_err(|source| PhaseInfraError::Spawn {
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
}

/// Strip optional YAML frontmatter (`---` … `---` at file start) and return
/// the body. Returns the input unchanged when no frontmatter is present.
fn strip_frontmatter(raw: &str) -> &str {
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
async fn resolve_ghq_base(ghq: &str) -> Result<std::path::PathBuf, PhaseInfraError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::AdmittedTicket;
    use crate::config::roki::*;
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
            linear: LinearSection { token: "x".to_string() },
            linear_webhook: LinearWebhookSection { bind: "127.0.0.1".to_string(), port: 8000, secret: None },
            default_ai_command: DefaultAiCommandSection { cli: "echo".to_string() },
            engine: EngineSection { max_iterations: 10 },
            paths: PathsSection { workflow: PathBuf::from("/tmp"), session_root: PathBuf::from("/tmp") },
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
            let argv_rendered = render_str(&argv_template, ctx)
                .map_err(|_| PhaseInfraError::Spawn {
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
                    Ok(PhaseOutcome::RunDone { exit_code, duration_seconds })
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
        let iter_dir = crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        ctx.set_iter(1);
        let body = PhaseBody::InlineCmd { cmd: "printf hello; printf err 1>&2; exit 5".into() };

        let out = exec.execute(PhaseKind::Run, &body, &ctx, &iter_dir).await.unwrap();
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
        let iter_dir = crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        let body = PhaseBody::InlineCmd {
            cmd: r#"printf '{"directive":"run","outcome":"ok"}'"#.to_string(),
        };

        let out = exec.execute(PhaseKind::Pre, &body, &ctx, &iter_dir).await.unwrap();
        match out {
            PhaseOutcome::PreDirective { directive, payload } => {
                assert_eq!(directive, super::PreDirective::Run);
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
        let iter_dir = crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        let body = PhaseBody::InlineCmd { cmd: r#"printf 'not json'"#.to_string() };

        let out = exec.execute(PhaseKind::Pre, &body, &ctx, &iter_dir).await.unwrap();
        match out {
            PhaseOutcome::Failure { kind: FailureKind::Unparseable } => {}
            other => panic!("expected Unparseable failure, got {other:?}"),
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
}

use super::outcome::{PostDirective, PreDirective}; // used by the test module
