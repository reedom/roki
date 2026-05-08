//! Cycle driver: iteration loop, transitions, iter cap.
//!
//! `run_cycle` consumes a `PhaseExecutor` (production or fake) so unit tests
//! exercise every directive transition deterministically. The daemon's
//! `runtime::run_inner` builds a `CommandPhaseExecutor` and passes it in.

use std::path::Path;

use uuid::Uuid;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::config::workflow::Rule;
use crate::error::{CaptureError, PhaseInfraError};

use super::context::PhaseContext;
use super::outcome::{
    CycleKind, FailureKind, FailureMeta, PhaseBody, PhaseKind, PhaseOutcome, PhaseShape,
    PostDirective, PreDirective,
};
use super::phase::PhaseExecutor;
use super::session::{SessionShutdownReason, SessionSupervisor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed { iters: u32, cycle_id: uuid::Uuid },
    Failed { meta: FailureMeta },
}

/// Convert a `CaptureError` that occurred before subprocess launch into a
/// `CycleOutcome::Failed` with `FailureKind::FsPoison`. Used at every site
/// where a `PhaseInfraError::Capture` surfaces from file-open or dir-create
/// calls before any process output exists.
fn fs_poison_outcome(
    capture_err: CaptureError,
    cycle_id: uuid::Uuid,
    phase: PhaseKind,
    iter: u32,
) -> CycleOutcome {
    CycleOutcome::Failed {
        meta: FailureMeta {
            failed_cycle_id: cycle_id,
            kind: FailureKind::FsPoison,
            phase,
            iter,
            exit_code: None,
            error_text: format!("session_tempdir creation failed: {capture_err}"),
        },
    }
}

/// Convert a `WorktreeError` raised before run-phase launch into a
/// `CycleOutcome::Failed` with `FailureKind::FsPoison`. The `phase` is
/// always `Run` at the call site (the only worktree-ensure point in the
/// cycle is between PreDirective::Run and run spawn).
fn worktree_fs_poison_outcome(
    err: crate::engine::worktree::WorktreeError,
    cycle_id: uuid::Uuid,
    iter: u32,
) -> CycleOutcome {
    let exit_code = err.exit_code();
    CycleOutcome::Failed {
        meta: FailureMeta {
            failed_cycle_id: cycle_id,
            kind: FailureKind::FsPoison,
            phase: PhaseKind::Run,
            iter,
            exit_code,
            error_text: format!("worktree ensure failed: {err}"),
        },
    }
}

/// Return the tail (up to `max` bytes) of `s`, prefixed with `...` when
/// truncated. Used to build operator-facing `error_text` from on-disk stderr.
fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let target = s.len().saturating_sub(max);
    let start = (target..=s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    format!("...{}", &s[start..])
}

/// Drive one cycle to completion or failure.
///
/// The executor parameter handles command-shape phases (`PhaseShape::Command`).
/// Session-shape phases (`PhaseShape::Session`) bypass the executor and route
/// through a `SessionSupervisor` constructed lazily here when any pre/post
/// resolves to session shape. Run is always command-shape per slice-2 invariant.
pub async fn run_cycle(
    executor: &dyn PhaseExecutor,
    admitted: &AdmittedTicket,
    rule: &Rule,
    session_root: &Path,
    cfg: &RokiConfig,
    cycle_kind: CycleKind,
    failure: Option<FailureMeta>,
) -> Result<CycleOutcome, PhaseInfraError> {
    let cycle_id = Uuid::new_v4();
    let mut ctx = PhaseContext::new(admitted, cycle_id, cfg, cycle_kind);
    if let Some(meta) = failure.clone() {
        ctx.set_failure(meta);
    }
    let max_iter = cfg.engine.max_iterations;
    let ticket_id = admitted.ticket.id.clone();
    let mut skip_pre = false;

    // Decide whether a SessionSupervisor is required for this cycle. The
    // supervisor is constructed once and reused across iterations because the
    // session subprocess is long-lived (slice-2 invariant: pre/post turns
    // share one stdin/stdout pipe).
    let pre_is_session = matches!(rule.pre.as_ref(), Some(b) if b.shape() == PhaseShape::Session);
    let post_is_session = matches!(rule.post.as_ref(), Some(b) if b.shape() == PhaseShape::Session);
    let needs_session = pre_is_session || post_is_session;

    let supervisor = if needs_session {
        // Resolve cwd via the same ghq path the command-shape executor uses.
        // The executor resolves it lazily per-call; the supervisor must have
        // it up-front because spawn happens before the first phase invocation.
        let cwd = crate::engine::cwd::resolve(&ctx.repo.ghq, &ticket_id).await?;
        let session_cfg = build_session_config(cfg, &ctx, &cwd)?;
        Some(SessionSupervisor::spawn(session_cfg).await?)
    } else {
        None
    };

    // Single-exit epilogue: every early-return assigns into `cycle_outcome`
    // and breaks to the shutdown step so the supervisor (if any) is always
    // wound down with the correct reason.
    let cycle_outcome: Result<CycleOutcome, PhaseInfraError> = 'cycle: {
        for iter in 1..=max_iter {
            ctx.set_iter(iter);
            let iter_dir =
                match crate::capture::create_iter_dir(session_root, &ticket_id, cycle_id, iter) {
                    Ok(d) => d,
                    Err(err) => {
                        break 'cycle Ok(fs_poison_outcome(err, cycle_id, PhaseKind::Pre, iter));
                    }
                };

            // Pre.
            if let Some(pre_body) = rule.pre.as_ref() {
                if !skip_pre {
                    let outcome = match run_phase_dispatch(
                        executor,
                        supervisor.as_ref(),
                        PhaseKind::Pre,
                        pre_body,
                        &ctx,
                        &iter_dir,
                    )
                    .await
                    {
                        Ok(o) => o,
                        Err(PhaseInfraError::Capture(e)) => {
                            break 'cycle Ok(fs_poison_outcome(e, cycle_id, PhaseKind::Pre, iter));
                        }
                        Err(err) => break 'cycle Err(err),
                    };
                    match outcome {
                        PhaseOutcome::Failure { kind } => {
                            let stderr_text = std::fs::read_to_string(iter_dir.join("pre.stderr"))
                                .unwrap_or_default();
                            break 'cycle Ok(CycleOutcome::Failed {
                                meta: FailureMeta {
                                    failed_cycle_id: cycle_id,
                                    kind,
                                    phase: PhaseKind::Pre,
                                    iter,
                                    exit_code: None,
                                    error_text: truncate_tail(&stderr_text, 4096),
                                },
                            });
                        }
                        PhaseOutcome::PreDirective {
                            directive: PreDirective::End,
                            payload,
                        } => {
                            ctx.set_pre(payload);
                            break 'cycle Ok(CycleOutcome::Completed {
                                iters: iter,
                                cycle_id,
                            });
                        }
                        PhaseOutcome::PreDirective {
                            directive: PreDirective::Run,
                            payload,
                        } => {
                            ctx.set_pre(payload);
                            // Lazy worktree materialization (fr:05). Errors here are
                            // pre-launch fs failures; route through FsPoison and let the
                            // runtime's [[on_failure]] dispatcher pick them up.
                            if let Err(err) =
                                crate::engine::worktree::ensure(&ctx.repo.ghq, &ticket_id).await
                            {
                                break 'cycle Ok(worktree_fs_poison_outcome(err, cycle_id, iter));
                            }
                        }
                        other => {
                            let got_variant = other.variant_name();
                            tracing::error!(
                                phase = "pre",
                                iter,
                                got_variant,
                                "phase executor returned outcome variant that does not belong to the requested phase"
                            );
                            break 'cycle Err(PhaseInfraError::ExecutorContract {
                                phase: PhaseKind::Pre,
                                got_variant,
                                iter,
                            });
                        }
                    }
                }
            }

            // Run. Always command-shape per slice-2 invariant — call the
            // executor directly rather than through the dispatch helper.
            let run_outcome = match executor
                .execute(PhaseKind::Run, &rule.run, &ctx, &iter_dir)
                .await
            {
                Ok(o) => o,
                Err(PhaseInfraError::Capture(e)) => {
                    break 'cycle Ok(fs_poison_outcome(e, cycle_id, PhaseKind::Run, iter));
                }
                Err(err) => break 'cycle Err(err),
            };
            match run_outcome {
                PhaseOutcome::Failure { kind } => {
                    let stderr_text =
                        std::fs::read_to_string(iter_dir.join("run.stderr")).unwrap_or_default();
                    break 'cycle Ok(CycleOutcome::Failed {
                        meta: FailureMeta {
                            failed_cycle_id: cycle_id,
                            kind,
                            phase: PhaseKind::Run,
                            iter,
                            exit_code: None,
                            error_text: truncate_tail(&stderr_text, 4096),
                        },
                    });
                }
                PhaseOutcome::RunDone {
                    exit_code,
                    duration_seconds,
                } => {
                    // Read the streamed run.terminal.json (written mid-stream
                    // by `tee_stdout` if a claude/codex `result` event was
                    // observed). Absent file or unparseable JSON simply leaves
                    // `terminal` as None — downstream phases consult it via
                    // `{{ run.terminal.* }}` and Liquid renders Nil to "".
                    let terminal = std::fs::read_to_string(iter_dir.join("run.terminal.json"))
                        .ok()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                    ctx.set_run(exit_code, duration_seconds, terminal);
                }
                other => {
                    let got_variant = other.variant_name();
                    tracing::error!(
                        phase = "run",
                        iter,
                        got_variant,
                        "phase executor returned outcome variant that does not belong to the requested phase"
                    );
                    break 'cycle Err(PhaseInfraError::ExecutorContract {
                        phase: PhaseKind::Run,
                        got_variant,
                        iter,
                    });
                }
            }

            // Post.
            let next = if let Some(post_body) = rule.post.as_ref() {
                let outcome = match run_phase_dispatch(
                    executor,
                    supervisor.as_ref(),
                    PhaseKind::Post,
                    post_body,
                    &ctx,
                    &iter_dir,
                )
                .await
                {
                    Ok(o) => o,
                    Err(PhaseInfraError::Capture(e)) => {
                        break 'cycle Ok(fs_poison_outcome(e, cycle_id, PhaseKind::Post, iter));
                    }
                    Err(err) => break 'cycle Err(err),
                };
                match outcome {
                    PhaseOutcome::Failure { kind } => {
                        let stderr_text = std::fs::read_to_string(iter_dir.join("post.stderr"))
                            .unwrap_or_default();
                        break 'cycle Ok(CycleOutcome::Failed {
                            meta: FailureMeta {
                                failed_cycle_id: cycle_id,
                                kind,
                                phase: PhaseKind::Post,
                                iter,
                                exit_code: None,
                                error_text: truncate_tail(&stderr_text, 4096),
                            },
                        });
                    }
                    PhaseOutcome::PostDirective { directive, payload } => {
                        ctx.set_post(payload);
                        directive
                    }
                    other => {
                        let got_variant = other.variant_name();
                        tracing::error!(
                            phase = "post",
                            iter,
                            got_variant,
                            "phase executor returned outcome variant that does not belong to the requested phase"
                        );
                        break 'cycle Err(PhaseInfraError::ExecutorContract {
                            phase: PhaseKind::Post,
                            got_variant,
                            iter,
                        });
                    }
                }
            } else {
                PostDirective::End
            };

            match next {
                PostDirective::End => {
                    break 'cycle Ok(CycleOutcome::Completed {
                        iters: iter,
                        cycle_id,
                    });
                }
                PostDirective::Pre => {
                    if iter == max_iter {
                        break 'cycle Ok(CycleOutcome::Failed {
                            meta: FailureMeta {
                                failed_cycle_id: cycle_id,
                                kind: FailureKind::IterExhausted,
                                phase: PhaseKind::Post,
                                iter,
                                exit_code: None,
                                error_text: format!(
                                    "iter {iter} exceeded max_iterations {max_iter}"
                                ),
                            },
                        });
                    }
                    skip_pre = false;
                }
                PostDirective::Run => {
                    if iter == max_iter {
                        break 'cycle Ok(CycleOutcome::Failed {
                            meta: FailureMeta {
                                failed_cycle_id: cycle_id,
                                kind: FailureKind::IterExhausted,
                                phase: PhaseKind::Post,
                                iter,
                                exit_code: None,
                                error_text: format!(
                                    "iter {iter} exceeded max_iterations {max_iter}"
                                ),
                            },
                        });
                    }
                    skip_pre = true;
                }
            }
        }

        // Unreachable: every transition either continues, returns Completed,
        // or returns IterExhausted. Defensive return for type completeness.
        Ok(CycleOutcome::Failed {
            meta: FailureMeta {
                failed_cycle_id: cycle_id,
                kind: FailureKind::IterExhausted,
                phase: PhaseKind::Post,
                iter: max_iter,
                exit_code: None,
                error_text: format!("iter {max_iter} exceeded max_iterations {max_iter}"),
            },
        })
    };

    // Wind the supervisor down with a reason matched to the cycle outcome.
    // Slice-2 contract: every cycle exit path closes the long-lived child.
    if let Some(sup) = supervisor.as_ref() {
        let reason = match &cycle_outcome {
            Ok(CycleOutcome::Completed { .. }) => SessionShutdownReason::Completed,
            Ok(CycleOutcome::Failed { meta }) if meta.kind == FailureKind::IterExhausted => {
                SessionShutdownReason::IterExhausted
            }
            _ => SessionShutdownReason::Failed,
        };
        sup.shutdown(reason).await;
    }

    cycle_outcome
}

/// Dispatch one phase invocation to either the command-shape executor or the
/// session supervisor based on `body.shape()`. Run is never routed here per
/// slice-2 invariant — the cycle calls `executor.execute` for Run directly.
async fn run_phase_dispatch(
    executor: &dyn PhaseExecutor,
    supervisor: Option<&SessionSupervisor>,
    kind: PhaseKind,
    body: &PhaseBody,
    ctx: &PhaseContext,
    iter_dir: &Path,
) -> Result<PhaseOutcome, PhaseInfraError> {
    match body.shape() {
        PhaseShape::Command => executor.execute(kind, body, ctx, iter_dir).await,
        PhaseShape::Session => {
            let sup = supervisor.expect(
                "session-shape phase but supervisor not constructed (cycle::run_cycle bug)",
            );
            let stdin_string = render_session_phase_body(body, ctx)?;
            sup.run_turn(
                iter_dir,
                kind,
                stdin_string.as_bytes(),
                body.stall_seconds_override(),
            )
            .await
        }
    }
}

/// Render the session turn's stdin body. Mirrors the command-shape stdin path:
/// inline-prompt is rendered verbatim; path-form reads the file, strips
/// frontmatter via the shared slice-1 helper, then renders. InlineCmd is
/// command-shape and unreachable here.
fn render_session_phase_body(
    body: &PhaseBody,
    ctx: &PhaseContext,
) -> Result<String, PhaseInfraError> {
    use crate::engine::template::render_str;

    let template = match body {
        PhaseBody::InlineCmd { .. } => unreachable!("inline cmd is command-shape"),
        PhaseBody::InlinePrompt { prompt } => prompt.clone(),
        PhaseBody::Path { path, .. } => {
            let raw = std::fs::read_to_string(path).map_err(|source| {
                PhaseInfraError::WorkflowBodyUnreadable {
                    path: path.clone(),
                    source,
                }
            })?;
            crate::engine::phase::strip_frontmatter(&raw).to_string()
        }
    };
    render_str(&template, ctx).map_err(|err| {
        // Liquid render failure surfaces as an infra error so the cycle
        // short-circuits with a typed Err and the operator log identifies
        // which template tripped the renderer. Wrapping under
        // WorkflowBodyUnreadable keeps the error count small without adding
        // a new variant just for session render failure.
        PhaseInfraError::WorkflowBodyUnreadable {
            path: std::path::PathBuf::from("<liquid render: session phase body>"),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()),
        }
    })
}

/// Build a `SessionConfig` from `cfg.default_ai_session` plus the rendered
/// cli template. Splits the rendered cli with `shell_words` and seeds the
/// child env with `ROKI_*` plus a small passthrough set (PATH/HOME/USER) —
/// same shape the command executor uses.
fn build_session_config(
    cfg: &RokiConfig,
    ctx: &PhaseContext,
    cwd: &Path,
) -> Result<crate::engine::session::SessionConfig, PhaseInfraError> {
    use crate::engine::context::roki_env_pairs;
    use crate::engine::template::render_str;

    let session_section = cfg
        .default_ai_session
        .as_ref()
        .ok_or(PhaseInfraError::SessionCliMissing)?;
    let cli_template = session_section
        .cli
        .as_deref()
        .ok_or(PhaseInfraError::SessionCliMissing)?;
    let rendered_cli =
        render_str(cli_template, ctx).map_err(|_| PhaseInfraError::SessionCliMissing)?;
    let argv = shell_words::split(&rendered_cli).map_err(|_| PhaseInfraError::SessionCliMissing)?;
    if argv.is_empty() {
        return Err(PhaseInfraError::SessionCliMissing);
    }

    let mut envs: Vec<(String, String)> = roki_env_pairs(ctx);
    for var in ["PATH", "HOME", "USER"] {
        if let Ok(val) = std::env::var(var) {
            envs.push((var.to_string(), val));
        }
    }

    Ok(crate::engine::session::SessionConfig {
        cli: rendered_cli,
        argv,
        default_stall_seconds: session_section.stall_seconds,
        cwd: cwd.to_path_buf(),
        envs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    use crate::admission::AdmittedTicket;
    use crate::config::roki::*;
    use crate::engine::outcome::{PhaseBody, PreDirective};
    use crate::linear::ticket::NormalizedTicket;
    use std::path::PathBuf;

    fn admitted() -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "ENG-CYC".to_string(),
                Some("u1".to_string()),
                "in_progress".to_string(),
                vec![],
                "T".to_string(),
                "B".to_string(),
            ),
            ghq: "github.com/acme/widget".to_string(),
        }
    }

    fn cfg(max_iter: u32) -> RokiConfig {
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
            engine: EngineSection {
                max_iterations: max_iter,
            },
            paths: PathsSection {
                workflow: PathBuf::from("/tmp/w"),
                session_root: PathBuf::from("/tmp/s"),
            },
            log: LogSection::default(),
            default_ai_session: None,
        }
    }

    fn rule(pre: Option<PhaseBody>, post: Option<PhaseBody>) -> Rule {
        Rule {
            when_status: "in_progress".to_string(),
            when_labels_has_all: vec![],
            pre,
            run: PhaseBody::InlineCmd {
                cmd: "true".to_string(),
            },
            post,
        }
    }

    /// Fake executor. Returns canned outcomes per (iter, phase). Records calls.
    struct FakeExec {
        scripted: Mutex<Vec<(u32, PhaseKind, PhaseOutcome)>>,
        calls: Mutex<Vec<(u32, PhaseKind)>>,
    }

    impl FakeExec {
        fn new(scripted: Vec<(u32, PhaseKind, PhaseOutcome)>) -> Self {
            Self {
                scripted: Mutex::new(scripted),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl PhaseExecutor for FakeExec {
        async fn execute(
            &self,
            kind: PhaseKind,
            _body: &PhaseBody,
            ctx: &PhaseContext,
            _iter_dir: &Path,
        ) -> Result<PhaseOutcome, PhaseInfraError> {
            self.calls.lock().unwrap().push((ctx.cycle.iter, kind));
            let mut scripted = self.scripted.lock().unwrap();
            let pos = scripted
                .iter()
                .position(|(i, k, _)| *i == ctx.cycle.iter && *k == kind)
                .unwrap_or_else(|| {
                    panic!("no scripted outcome for ({}, {:?})", ctx.cycle.iter, kind)
                });
            let (_, _, out) = scripted.remove(pos);
            Ok(out)
        }
    }

    #[tokio::test]
    async fn pre_end_short_circuits_before_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Pre,
            PhaseOutcome::PreDirective {
                directive: PreDirective::End,
                payload: serde_json::json!({"directive":"end"}),
            },
        )]);
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);
        let outcome = run_cycle(
            &exec,
            &admitted(),
            &r,
            tmp.path(),
            &cfg(10),
            CycleKind::Rule,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CycleOutcome::Completed { iters: 1, .. }));
        let calls = exec.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(1, PhaseKind::Pre)]);
    }

    #[tokio::test]
    async fn full_iter_pre_run_post_end() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(&wt_root).unwrap();
        let exec = FakeExec::new(vec![
            (
                1,
                PhaseKind::Pre,
                PhaseOutcome::PreDirective {
                    directive: PreDirective::Run,
                    payload: serde_json::json!({"directive":"run"}),
                },
            ),
            (
                1,
                PhaseKind::Run,
                PhaseOutcome::RunDone {
                    exit_code: 0,
                    duration_seconds: 1,
                },
            ),
            (
                1,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::End,
                    payload: serde_json::json!({"directive":"end"}),
                },
            ),
        ]);
        let r = rule(
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
        );
        let outcome = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
            run_cycle(
                &exec,
                &admitted(),
                &r,
                tmp.path(),
                &cfg(10),
                CycleKind::Rule,
                None,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CycleOutcome::Completed { iters: 1, .. }));
    }

    #[tokio::test]
    async fn post_run_skips_pre_in_next_iteration() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(&wt_root).unwrap();
        let exec = FakeExec::new(vec![
            (
                1,
                PhaseKind::Pre,
                PhaseOutcome::PreDirective {
                    directive: PreDirective::Run,
                    payload: serde_json::json!({}),
                },
            ),
            (
                1,
                PhaseKind::Run,
                PhaseOutcome::RunDone {
                    exit_code: 0,
                    duration_seconds: 0,
                },
            ),
            (
                1,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::Run,
                    payload: serde_json::json!({}),
                },
            ),
            (
                2,
                PhaseKind::Run,
                PhaseOutcome::RunDone {
                    exit_code: 0,
                    duration_seconds: 0,
                },
            ),
            (
                2,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::End,
                    payload: serde_json::json!({}),
                },
            ),
        ]);
        let r = rule(
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
        );
        let outcome = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
            run_cycle(
                &exec,
                &admitted(),
                &r,
                tmp.path(),
                &cfg(10),
                CycleKind::Rule,
                None,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CycleOutcome::Completed { iters: 2, .. }));
        let calls = exec.calls.lock().unwrap().clone();
        let pre_iter2 = calls.iter().find(|(i, k)| *i == 2 && *k == PhaseKind::Pre);
        assert!(
            pre_iter2.is_none(),
            "iter 2 pre must be skipped, calls: {calls:?}"
        );
    }

    #[tokio::test]
    async fn iter_cap_with_post_run_yields_iter_exhausted() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (
                1,
                PhaseKind::Run,
                PhaseOutcome::RunDone {
                    exit_code: 0,
                    duration_seconds: 0,
                },
            ),
            (
                1,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::Run,
                    payload: serde_json::json!({}),
                },
            ),
            (
                2,
                PhaseKind::Run,
                PhaseOutcome::RunDone {
                    exit_code: 0,
                    duration_seconds: 0,
                },
            ),
            (
                2,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::Run,
                    payload: serde_json::json!({}),
                },
            ),
        ]);
        let r = rule(None, Some(PhaseBody::InlineCmd { cmd: "true".into() }));
        let outcome = run_cycle(
            &exec,
            &admitted(),
            &r,
            tmp.path(),
            &cfg(2),
            CycleKind::Rule,
            None,
        )
        .await
        .unwrap();
        match outcome {
            CycleOutcome::Failed { meta } => {
                assert_eq!(meta.kind, FailureKind::IterExhausted);
                assert_eq!(meta.iter, 2);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_absent_terminates_after_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Run,
            PhaseOutcome::RunDone {
                exit_code: 0,
                duration_seconds: 0,
            },
        )]);
        let r = rule(None, None);
        let outcome = run_cycle(
            &exec,
            &admitted(),
            &r,
            tmp.path(),
            &cfg(10),
            CycleKind::Rule,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CycleOutcome::Completed { iters: 1, .. }));
    }

    #[tokio::test]
    async fn pre_absent_starts_at_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (
                1,
                PhaseKind::Run,
                PhaseOutcome::RunDone {
                    exit_code: 0,
                    duration_seconds: 0,
                },
            ),
            (
                1,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::End,
                    payload: serde_json::json!({}),
                },
            ),
        ]);
        let r = rule(None, Some(PhaseBody::InlineCmd { cmd: "true".into() }));
        let outcome = run_cycle(
            &exec,
            &admitted(),
            &r,
            tmp.path(),
            &cfg(10),
            CycleKind::Rule,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CycleOutcome::Completed { iters: 1, .. }));
    }

    #[tokio::test]
    async fn executor_returning_wrong_variant_yields_typed_infra_error() {
        // Misbehaving executor returns a Run-shaped outcome from a Pre call.
        // Cycle driver must surface this as a typed PhaseInfraError rather
        // than panicking, so a single buggy phase impl cannot crash the
        // daemon process.
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Pre,
            PhaseOutcome::RunDone {
                exit_code: 0,
                duration_seconds: 0,
            },
        )]);
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);
        let err = run_cycle(
            &exec,
            &admitted(),
            &r,
            tmp.path(),
            &cfg(10),
            CycleKind::Rule,
            None,
        )
        .await
        .expect_err("wrong variant must surface as Err");
        match err {
            PhaseInfraError::ExecutorContract {
                phase,
                got_variant,
                iter,
            } => {
                assert_eq!(phase, PhaseKind::Pre);
                assert_eq!(got_variant, "RunDone");
                assert_eq!(iter, 1);
            }
            other => panic!("expected ExecutorContract, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_directive_pre_runs_pre_again_on_next_iteration() {
        // PostDirective::Pre must clear skip_pre so iter 2 re-runs the pre
        // phase. Symmetrical to the post-Run test above.
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(&wt_root).unwrap();
        let exec = FakeExec::new(vec![
            (
                1,
                PhaseKind::Pre,
                PhaseOutcome::PreDirective {
                    directive: PreDirective::Run,
                    payload: serde_json::json!({}),
                },
            ),
            (
                1,
                PhaseKind::Run,
                PhaseOutcome::RunDone {
                    exit_code: 0,
                    duration_seconds: 0,
                },
            ),
            (
                1,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::Pre,
                    payload: serde_json::json!({}),
                },
            ),
            (
                2,
                PhaseKind::Pre,
                PhaseOutcome::PreDirective {
                    directive: PreDirective::Run,
                    payload: serde_json::json!({}),
                },
            ),
            (
                2,
                PhaseKind::Run,
                PhaseOutcome::RunDone {
                    exit_code: 0,
                    duration_seconds: 0,
                },
            ),
            (
                2,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::End,
                    payload: serde_json::json!({}),
                },
            ),
        ]);
        let r = rule(
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
        );
        let outcome = temp_env::async_with_vars(
            [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
            run_cycle(
                &exec,
                &admitted(),
                &r,
                tmp.path(),
                &cfg(10),
                CycleKind::Rule,
                None,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, CycleOutcome::Completed { iters: 2, .. }));
        let calls = exec.calls.lock().unwrap().clone();
        let pre_iter2 = calls.iter().find(|(i, k)| *i == 2 && *k == PhaseKind::Pre);
        assert!(
            pre_iter2.is_some(),
            "iter 2 pre must run after PostDirective::Pre, calls: {calls:?}"
        );
    }

    #[tokio::test]
    async fn pre_failure_returns_failed_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Pre,
            PhaseOutcome::Failure {
                kind: FailureKind::Unparseable,
            },
        )]);
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);
        let outcome = run_cycle(
            &exec,
            &admitted(),
            &r,
            tmp.path(),
            &cfg(10),
            CycleKind::Rule,
            None,
        )
        .await
        .unwrap();
        match outcome {
            CycleOutcome::Failed { meta } => {
                assert_eq!(meta.kind, FailureKind::Unparseable);
                assert_eq!(meta.iter, 1);
                assert_eq!(meta.phase, PhaseKind::Pre);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // Note on session-dispatch test: a fully-realised unit test that drives
    // run_cycle through `SessionSupervisor::spawn` requires either a real
    // ghq tree on disk or in-process mutation of `ROKI_GHQ_BASE_OVERRIDE`
    // (rejected by this crate's `-F unsafe-code` lint at edition 2024).
    // The end-to-end coverage for session dispatch is provided by the
    // Task 19 e2e smoke test, which spawns the daemon binary in a child
    // process where the env override is set safely via `Command::env`.

    #[test]
    fn cycle_outcome_failed_carries_meta() {
        let id = uuid::Uuid::nil();
        let meta = crate::engine::outcome::FailureMeta {
            failed_cycle_id: id,
            kind: crate::engine::outcome::FailureKind::IterExhausted,
            phase: crate::engine::outcome::PhaseKind::Post,
            iter: 5,
            exit_code: None,
            error_text: "iter 5 exceeded cap".into(),
        };
        let outcome = crate::engine::CycleOutcome::Failed { meta };
        match outcome {
            crate::engine::CycleOutcome::Failed { meta } => {
                assert_eq!(meta.iter, 5);
                assert_eq!(meta.kind.as_str(), "iter_exhausted");
            }
            _ => panic!("expected Failed"),
        }
    }

    #[tokio::test]
    async fn fs_poison_when_session_root_unwritable() {
        // iter_dir creation fails because the session_root path has a file
        // component where a directory is required. This exercises the
        // create_iter_dir → FsPoison conversion in run_cycle.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        // Pass the blocker file as session_root so create_iter_dir fails.
        let bad_root = blocker.as_path();
        let exec = FakeExec::new(vec![]);
        let r = rule(None, None);
        let outcome = run_cycle(
            &exec,
            &admitted(),
            &r,
            bad_root,
            &cfg(10),
            CycleKind::Rule,
            None,
        )
        .await
        .unwrap();
        match outcome {
            CycleOutcome::Failed { meta } => {
                assert_eq!(meta.kind, FailureKind::FsPoison);
                assert_eq!(meta.iter, 1);
                assert!(meta.exit_code.is_none());
                assert!(meta.error_text.contains("session_tempdir"));
            }
            other => panic!("expected FsPoison; got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fs_poison_when_phase_capture_open_fails() {
        // FailingCaptureExec simulates a per-phase capture file open failure,
        // exactly as CommandPhaseExecutor would surface it when open_phase_files
        // returns CaptureError::OpenFile before the subprocess is launched.
        struct FailingCaptureExec;

        #[async_trait]
        impl PhaseExecutor for FailingCaptureExec {
            async fn execute(
                &self,
                _kind: PhaseKind,
                _body: &PhaseBody,
                _ctx: &PhaseContext,
                _iter_dir: &Path,
            ) -> Result<PhaseOutcome, PhaseInfraError> {
                Err(PhaseInfraError::Capture(
                    crate::error::CaptureError::OpenFile {
                        path: PathBuf::from("/tmp/fake/pre.stdout"),
                        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
                    },
                ))
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let exec = FailingCaptureExec;
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);

        let outcome = run_cycle(
            &exec,
            &admitted(),
            &r,
            tmp.path(),
            &cfg(10),
            CycleKind::Rule,
            None,
        )
        .await
        .unwrap();

        match outcome {
            CycleOutcome::Failed { meta } => {
                assert_eq!(meta.kind, FailureKind::FsPoison);
                assert_eq!(meta.phase, PhaseKind::Pre);
                assert_eq!(meta.iter, 1);
                assert!(meta.exit_code.is_none());
                assert!(meta.error_text.contains("session_tempdir"));
            }
            other => panic!("expected FsPoison; got {other:?}"),
        }
    }

    #[test]
    fn truncate_tail_handles_multi_byte_utf8_at_boundary() {
        // Build a string where the computed start index will fall in the middle of a multi-byte char.
        // We need total_len - max to land on a byte that's not a char boundary.
        // A 4-byte emoji (🦀) followed by ASCII: if we have 4100 bytes and max=4096,
        // start=4. If the last 4 bytes before position 4100 are a 4-byte emoji starting at 4096,
        // then trying to slice from index 4 would fail if it's mid-emoji.
        // Simpler: make a string with padding + emoji where truncation lands mid-emoji.
        let mut s = String::new();
        // Write enough ASCII to reach a position where truncation will hit an emoji badly.
        // We want: total_bytes - max to be non-char-boundary.
        // If max=10 and we have 15 bytes with the last 4 being emoji, start=5.
        // Position 5 might be in the middle of the emoji if emoji starts earlier.

        // Create: "aaaaaaaaaa🦀" = 10 bytes ASCII + 4 bytes emoji = 14 bytes total
        for _ in 0..10 {
            s.push('a');
        }
        s.push('🦀');

        // Now truncate to max=12: should try to slice from index 14-12=2.
        // Index 2 is a valid char boundary (it's in the ASCII part).
        // Let's adjust: make it so truncation lands in emoji.
        // "aaaaaaaaaa🦀" with max=11 means start=14-11=3, still valid.
        // We need the emoji to start earlier. Try: "aaaa🦀aaaaaa" = 4+4+6 = 14
        s.clear();
        for _ in 0..4 {
            s.push('a');
        }
        s.push('🦀');
        for _ in 0..6 {
            s.push('a');
        }
        // Now s has 14 bytes. With max=11, start=14-11=3.
        // Position 3 is between 'a' and emoji start, valid boundary.
        // We need a bigger emoji or different placement.

        // Better approach: use a scenario where byte arithmetic breaks.
        // "a🦀a🦀a🦀a🦀a" - each emoji is 4 bytes, alternating with ASCII.
        s.clear();
        s.push('a');
        s.push('🦀');
        s.push('a');
        s.push('🦀');
        s.push('a');
        s.push('🦀');
        s.push('a');
        s.push('🦀');
        s.push('a');
        // 5 'a's (5 bytes) + 4 emojis (16 bytes) = 21 bytes
        // With max=19, start=21-19=2. Pos 2 is between first 'a' and first emoji.
        // Still valid. Let's try max=18: start=3, which is the second byte of emoji at pos 1.

        let truncated = super::truncate_tail(&s, 18);
        // The old code would panic if start=3 is not a char boundary.
        // The new code should find the next char boundary and succeed.
        assert!(truncated.starts_with("..."));
        // Result should be valid UTF-8.
        let _ = truncated.chars().count(); // This would panic if invalid UTF-8
    }

    #[test]
    fn truncate_tail_short_string_passthrough() {
        assert_eq!(super::truncate_tail("hello", 100), "hello");
    }

    #[test]
    fn truncate_tail_ascii_at_max() {
        let s = "x".repeat(50);
        let out = super::truncate_tail(&s, 10);
        assert!(out.starts_with("..."));
        assert_eq!(out.len(), 10 + 3); // last 10 chars + "..." prefix
    }
}
