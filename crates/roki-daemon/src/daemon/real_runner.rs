#![allow(dead_code)]

//! Production `CycleRunner` impl bridging `daemon::ticket_task` to
//! `engine::cycle::run_cycle` and slice-3's `[[on_failure]]` routing.

use std::sync::Arc;

use uuid::Uuid;

use crate::escalation::EscalationQueue;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::config::workflow::{Cleanup, Rule, WorkflowConfig};
use crate::daemon::ticket_task::{CycleResult, CycleRunner};
use crate::engine::CommandPhaseExecutor;
use crate::engine::context::CycleTrigger;
use crate::engine::dispatch::DispatchTarget;
use crate::engine::outcome::{CycleKind, FailureKind, FailureMeta, PhaseKind};
use crate::events::{Event, EventWriter, FailureMarker, FailureMetaSer, now_rfc3339};

pub struct RealCycleRunner {
    pub workflow: Arc<WorkflowConfig>,
    pub cfg: Arc<RokiConfig>,
    pub executor: Arc<CommandPhaseExecutor>,
    pub escalation: Arc<EscalationQueue>,
}

#[async_trait::async_trait]
impl CycleRunner for RealCycleRunner {
    async fn run_cycle(
        &self,
        admitted: &AdmittedTicket,
        target: DispatchTarget<'_>,
        _cycle_id: Uuid,
        cycle_trigger: CycleTrigger,
    ) -> CycleResult {
        let mut events = match EventWriter::open(&self.cfg.paths.session_root, &admitted.ticket.id)
        {
            Ok(w) => w,
            Err(_) => {
                return CycleResult::Failed {
                    meta: boot_path_failure(),
                    kind: CycleKind::Rule,
                };
            }
        };

        let (rule_view, kind) = match target {
            DispatchTarget::Cycle {
                kind,
                rule: Some(r),
                ..
            } => (r.clone(), kind),
            DispatchTarget::Cycle {
                kind,
                cleanup: Some(c),
                ..
            } => (cleanup_to_rule(c), kind),
            DispatchTarget::CleanupShorthand => {
                let cycle_id = uuid::Uuid::new_v4();
                if crate::engine::cleanup::delete_immediate(
                    &admitted.ticket.id,
                    &admitted.ghq,
                    &self.cfg.paths.session_root,
                    cycle_id,
                    &mut events,
                    &self.escalation,
                )
                .await
                .is_err()
                {
                    return CycleResult::CleanupFsError {
                        ticket_id: admitted.ticket.id.clone(),
                    };
                }
                return CycleResult::ShorthandDeleted;
            }
            DispatchTarget::Cycle {
                rule: None,
                cleanup: None,
                ..
            }
            | DispatchTarget::NoMatch => {
                unreachable!("dispatcher only forwards matched targets")
            }
        };

        let outcome = match crate::engine::run_cycle(
            self.executor.as_ref(),
            admitted,
            &rule_view,
            &self.cfg.paths.session_root,
            self.cfg.as_ref(),
            kind,
            cycle_trigger,
            None,
        )
        .await
        {
            Ok(o) => o,
            Err(_e) => {
                return CycleResult::Failed {
                    meta: boot_path_failure(),
                    kind,
                };
            }
        };

        match outcome {
            crate::engine::CycleOutcome::Completed { iters, cycle_id } => {
                let _ = events.emit(&Event::CycleCompleted {
                    ts: now_rfc3339(),
                    cycle_id: cycle_id.to_string(),
                    cycle_kind: kind.as_str().to_string(),
                    iters,
                    outcome: None,
                });
                if kind == CycleKind::Cleanup {
                    let _ = crate::engine::cleanup::post_cycle_delete(
                        &admitted.ticket.id,
                        &admitted.ghq,
                        &self.cfg.paths.session_root,
                        cycle_id,
                        &mut events,
                        &self.escalation,
                    )
                    .await;
                }
                CycleResult::Completed { kind, iters }
            }
            crate::engine::CycleOutcome::Failed { meta } => {
                let decision = handle_failed_cycle(
                    &meta,
                    kind,
                    self.workflow.as_ref(),
                    self.executor.as_ref(),
                    admitted,
                    self.cfg.as_ref(),
                    &mut events,
                    cycle_trigger,
                    &self.escalation,
                )
                .await;
                match decision {
                    HandlerDecision::Succeeded => CycleResult::Completed {
                        kind: CycleKind::Failure,
                        iters: 0,
                    },
                    HandlerDecision::Unhandled => CycleResult::Failed { meta, kind },
                }
            }
        }
    }
}

enum HandlerDecision {
    Succeeded,
    Unhandled,
}

fn boot_path_failure() -> FailureMeta {
    FailureMeta {
        failed_cycle_id: uuid::Uuid::nil(),
        kind: FailureKind::FsPoison,
        phase: PhaseKind::Pre,
        iter: 0,
        exit_code: None,
        error_text: "runner boot path".into(),
    }
}

fn cleanup_to_rule(c: &Cleanup) -> Rule {
    Rule {
        when_status: c.when_status.clone().unwrap_or_default(),
        when_labels_has_all: c.when_labels_has_all.clone(),
        pre: c.pre.clone(),
        run: c.run.clone().expect("non-shorthand cleanup has run"),
        post: c.post.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_failed_cycle(
    meta: &FailureMeta,
    failed_kind: CycleKind,
    workflow: &WorkflowConfig,
    executor: &CommandPhaseExecutor,
    admitted: &AdmittedTicket,
    cfg: &RokiConfig,
    events: &mut EventWriter,
    cycle_trigger: CycleTrigger,
    escalation: &EscalationQueue,
) -> HandlerDecision {
    // Recursion bound: a failure cycle that itself fails must not recurse.
    // fr:06 trigger 1: push to escalation queue instead of emitting
    // failure_unhandled.
    if failed_kind == CycleKind::Failure {
        escalation
            .push_cycle(
                admitted.ticket.id.clone(),
                meta.failed_cycle_id,
                meta.kind,
                meta.phase,
                meta.error_text.clone(),
            )
            .await;
        return HandlerDecision::Unhandled;
    }

    // First-match against [[on_failure]].
    let Some(handler) = crate::engine::on_failure::route(&workflow.on_failures, meta) else {
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: meta.failed_cycle_id.to_string(),
            cycle_kind: failed_kind.as_str().to_string(),
            failure: FailureMetaSer::from_meta(meta),
            marker: FailureMarker::None,
        });
        return HandlerDecision::Unhandled;
    };

    let handler_rule = on_failure_to_rule(handler);
    match crate::engine::run_cycle(
        executor,
        admitted,
        &handler_rule,
        &cfg.paths.session_root,
        cfg,
        CycleKind::Failure,
        cycle_trigger,
        Some(meta.clone()),
    )
    .await
    {
        Ok(crate::engine::CycleOutcome::Completed { iters, cycle_id }) => {
            let _ = events.emit(&Event::CycleCompleted {
                ts: now_rfc3339(),
                cycle_id: cycle_id.to_string(),
                cycle_kind: "failure".into(),
                iters,
                outcome: None,
            });
            HandlerDecision::Succeeded
        }
        Ok(crate::engine::CycleOutcome::Failed { meta: handler_meta }) => {
            // fr:06 trigger 1: handler cycle failed. Push to queue with the
            // handler cycle's own id so operators can correlate logs.
            escalation
                .push_cycle(
                    admitted.ticket.id.clone(),
                    handler_meta.failed_cycle_id,
                    handler_meta.kind,
                    handler_meta.phase,
                    handler_meta.error_text.clone(),
                )
                .await;
            HandlerDecision::Unhandled
        }
        Err(infra) => {
            tracing::error!(?infra, "handler cycle infra error");
            // fr:06 trigger 1: handler cycle hit an infra error. Synthesize
            // FsPoison for the failure_kind because infra errors do not carry
            // a phase-level FailureKind. Tag with the original failed cycle's
            // id and phase to keep the operator-visible scope identical to
            // the user's [[on_failure]] match.
            escalation
                .push_cycle(
                    admitted.ticket.id.clone(),
                    meta.failed_cycle_id,
                    FailureKind::FsPoison,
                    meta.phase,
                    format!("handler cycle infra error: {infra}"),
                )
                .await;
            HandlerDecision::Unhandled
        }
    }
}

fn on_failure_to_rule(h: &crate::engine::on_failure::OnFailure) -> Rule {
    Rule {
        when_status: String::new(),
        when_labels_has_all: vec![],
        pre: h.pre.clone(),
        run: h.run.clone(),
        post: h.post.clone(),
    }
}
