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
use crate::error::PhaseInfraError;

use super::context::PhaseContext;
use super::outcome::{
    FailureKind, PhaseBody, PhaseKind, PhaseOutcome, PhaseShape, PostDirective, PreDirective,
};
use super::phase::PhaseExecutor;
use super::session::{SessionShutdownReason, SessionSupervisor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed { kind: FailureKind, iter: u32 },
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
) -> Result<CycleOutcome, PhaseInfraError> {
    let cycle_id = Uuid::new_v4();
    let mut ctx = PhaseContext::new(admitted, cycle_id, cfg);
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
        let cwd = crate::engine::phase::resolve_ghq_base(&ctx.repo.ghq).await?;
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
            let iter_dir = match crate::capture::create_iter_dir(
                session_root,
                &ticket_id,
                cycle_id,
                iter,
            ) {
                Ok(d) => d,
                Err(err) => break 'cycle Err(err.into()),
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
                        Err(err) => break 'cycle Err(err),
                    };
                    match outcome {
                        PhaseOutcome::Failure { kind } => {
                            break 'cycle Ok(CycleOutcome::Failed { kind, iter });
                        }
                        PhaseOutcome::PreDirective {
                            directive: PreDirective::End,
                            payload,
                        } => {
                            ctx.set_pre(payload);
                            break 'cycle Ok(CycleOutcome::Completed { iters: iter });
                        }
                        PhaseOutcome::PreDirective {
                            directive: PreDirective::Run,
                            payload,
                        } => {
                            ctx.set_pre(payload);
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
            let run_outcome =
                match executor.execute(PhaseKind::Run, &rule.run, &ctx, &iter_dir).await {
                    Ok(o) => o,
                    Err(err) => break 'cycle Err(err),
                };
            match run_outcome {
                PhaseOutcome::Failure { kind } => {
                    break 'cycle Ok(CycleOutcome::Failed { kind, iter });
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
                    Err(err) => break 'cycle Err(err),
                };
                match outcome {
                    PhaseOutcome::Failure { kind } => {
                        break 'cycle Ok(CycleOutcome::Failed { kind, iter });
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
                    break 'cycle Ok(CycleOutcome::Completed { iters: iter });
                }
                PostDirective::Pre => {
                    if iter == max_iter {
                        break 'cycle Ok(CycleOutcome::Failed {
                            kind: FailureKind::IterExhausted,
                            iter,
                        });
                    }
                    skip_pre = false;
                }
                PostDirective::Run => {
                    if iter == max_iter {
                        break 'cycle Ok(CycleOutcome::Failed {
                            kind: FailureKind::IterExhausted,
                            iter,
                        });
                    }
                    skip_pre = true;
                }
            }
        }

        // Unreachable: every transition either continues, returns Completed,
        // or returns IterExhausted. Defensive return for type completeness.
        Ok(CycleOutcome::Failed {
            kind: FailureKind::IterExhausted,
            iter: max_iter,
        })
    };

    // Wind the supervisor down with a reason matched to the cycle outcome.
    // Slice-2 contract: every cycle exit path closes the long-lived child.
    if let Some(sup) = supervisor.as_ref() {
        let reason = match &cycle_outcome {
            Ok(CycleOutcome::Completed { .. }) => SessionShutdownReason::Completed,
            Ok(CycleOutcome::Failed {
                kind: FailureKind::IterExhausted,
                ..
            }) => SessionShutdownReason::IterExhausted,
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
            let raw =
                std::fs::read_to_string(path).map_err(|source| {
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
    let argv =
        shell_words::split(&rendered_cli).map_err(|_| PhaseInfraError::SessionCliMissing)?;
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
            linear: LinearSection { token: "x".to_string() },
            linear_webhook: LinearWebhookSection {
                bind: "127.0.0.1".to_string(),
                port: 8000,
                secret: None,
            },
            default_ai_command: DefaultAiCommandSection { cli: "echo".to_string(), stall_seconds: 300 },
            engine: EngineSection { max_iterations: max_iter },
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
            run: PhaseBody::InlineCmd { cmd: "true".to_string() },
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
                .unwrap_or_else(|| panic!("no scripted outcome for ({}, {:?})", ctx.cycle.iter, kind));
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
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 1 });
        let calls = exec.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(1, PhaseKind::Pre)]);
    }

    #[tokio::test]
    async fn full_iter_pre_run_post_end() {
        let tmp = tempfile::tempdir().unwrap();
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
                PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 1 },
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
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 1 });
    }

    #[tokio::test]
    async fn post_run_skips_pre_in_next_iteration() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (1, PhaseKind::Pre, PhaseOutcome::PreDirective {
                directive: PreDirective::Run,
                payload: serde_json::json!({}),
            }),
            (1, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (1, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::Run,
                payload: serde_json::json!({}),
            }),
            (2, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (2, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::End,
                payload: serde_json::json!({}),
            }),
        ]);
        let r = rule(
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
        );
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 2 });
        let calls = exec.calls.lock().unwrap().clone();
        let pre_iter2 = calls.iter().find(|(i, k)| *i == 2 && *k == PhaseKind::Pre);
        assert!(pre_iter2.is_none(), "iter 2 pre must be skipped, calls: {calls:?}");
    }

    #[tokio::test]
    async fn iter_cap_with_post_run_yields_iter_exhausted() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (1, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (1, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::Run,
                payload: serde_json::json!({}),
            }),
            (2, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (2, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::Run,
                payload: serde_json::json!({}),
            }),
        ]);
        let r = rule(None, Some(PhaseBody::InlineCmd { cmd: "true".into() }));
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(2)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Failed { kind: FailureKind::IterExhausted, iter: 2 });
    }

    #[tokio::test]
    async fn post_absent_terminates_after_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Run,
            PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 },
        )]);
        let r = rule(None, None);
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 1 });
    }

    #[tokio::test]
    async fn pre_absent_starts_at_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (1, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (1, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::End,
                payload: serde_json::json!({}),
            }),
        ]);
        let r = rule(None, Some(PhaseBody::InlineCmd { cmd: "true".into() }));
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 1 });
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
            PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 },
        )]);
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);
        let err = run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10))
            .await
            .expect_err("wrong variant must surface as Err");
        match err {
            PhaseInfraError::ExecutorContract { phase, got_variant, iter } => {
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
        let exec = FakeExec::new(vec![
            (1, PhaseKind::Pre, PhaseOutcome::PreDirective {
                directive: PreDirective::Run,
                payload: serde_json::json!({}),
            }),
            (1, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (1, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::Pre,
                payload: serde_json::json!({}),
            }),
            (2, PhaseKind::Pre, PhaseOutcome::PreDirective {
                directive: PreDirective::Run,
                payload: serde_json::json!({}),
            }),
            (2, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (2, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::End,
                payload: serde_json::json!({}),
            }),
        ]);
        let r = rule(
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
        );
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 2 });
        let calls = exec.calls.lock().unwrap().clone();
        let pre_iter2 = calls.iter().find(|(i, k)| *i == 2 && *k == PhaseKind::Pre);
        assert!(pre_iter2.is_some(), "iter 2 pre must run after PostDirective::Pre, calls: {calls:?}");
    }

    #[tokio::test]
    async fn pre_failure_returns_failed_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Pre,
            PhaseOutcome::Failure { kind: FailureKind::Unparseable },
        )]);
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Failed { kind: FailureKind::Unparseable, iter: 1 });
    }

    // Note on session-dispatch test: a fully-realised unit test that drives
    // run_cycle through `SessionSupervisor::spawn` requires either a real
    // ghq tree on disk or in-process mutation of `ROKI_GHQ_BASE_OVERRIDE`
    // (rejected by this crate's `-F unsafe-code` lint at edition 2024).
    // The end-to-end coverage for session dispatch is provided by the
    // Task 19 e2e smoke test, which spawns the daemon binary in a child
    // process where the env override is set safely via `Command::env`.
}
