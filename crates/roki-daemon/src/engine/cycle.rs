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
    FailureKind, PhaseKind, PhaseOutcome, PostDirective, PreDirective,
};
use super::phase::PhaseExecutor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed { kind: FailureKind, iter: u32 },
}

/// Drive one cycle to completion or failure.
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

    for iter in 1..=max_iter {
        ctx.set_iter(iter);
        let iter_dir =
            crate::capture::create_iter_dir(session_root, &ticket_id, cycle_id, iter)?;

        // Pre.
        if let Some(pre_body) = rule.pre.as_ref() {
            if !skip_pre {
                match executor.execute(PhaseKind::Pre, pre_body, &ctx, &iter_dir).await? {
                    PhaseOutcome::Failure { kind } => {
                        return Ok(CycleOutcome::Failed { kind, iter });
                    }
                    PhaseOutcome::PreDirective {
                        directive: PreDirective::End,
                        payload,
                    } => {
                        ctx.set_pre(payload);
                        return Ok(CycleOutcome::Completed { iters: iter });
                    }
                    PhaseOutcome::PreDirective {
                        directive: PreDirective::Run,
                        payload,
                    } => {
                        ctx.set_pre(payload);
                    }
                    other => panic!("Pre executor returned non-Pre outcome: {other:?}"),
                }
            }
        }

        // Run.
        match executor.execute(PhaseKind::Run, &rule.run, &ctx, &iter_dir).await? {
            PhaseOutcome::Failure { kind } => {
                return Ok(CycleOutcome::Failed { kind, iter });
            }
            PhaseOutcome::RunDone {
                exit_code,
                duration_seconds,
            } => {
                ctx.set_run(exit_code, duration_seconds);
            }
            other => panic!("Run executor returned non-Run outcome: {other:?}"),
        }

        // Post.
        let next = if let Some(post_body) = rule.post.as_ref() {
            match executor.execute(PhaseKind::Post, post_body, &ctx, &iter_dir).await? {
                PhaseOutcome::Failure { kind } => {
                    return Ok(CycleOutcome::Failed { kind, iter });
                }
                PhaseOutcome::PostDirective { directive, payload } => {
                    ctx.set_post(payload);
                    directive
                }
                other => panic!("Post executor returned non-Post outcome: {other:?}"),
            }
        } else {
            PostDirective::End
        };

        match next {
            PostDirective::End => return Ok(CycleOutcome::Completed { iters: iter }),
            PostDirective::Pre => {
                if iter == max_iter {
                    return Ok(CycleOutcome::Failed { kind: FailureKind::IterExhausted, iter });
                }
                skip_pre = false;
            }
            PostDirective::Run => {
                if iter == max_iter {
                    return Ok(CycleOutcome::Failed { kind: FailureKind::IterExhausted, iter });
                }
                skip_pre = true;
            }
        }
    }

    // Unreachable: every transition either continues, returns Completed, or
    // returns IterExhausted. Defensive return for type completeness.
    Ok(CycleOutcome::Failed {
        kind: FailureKind::IterExhausted,
        iter: max_iter,
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
            default_ai_command: DefaultAiCommandSection { cli: "echo".to_string() },
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
}
