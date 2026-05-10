#![allow(dead_code)]

//! Production `CycleRunner` impl bridging `daemon::ticket_task` to
//! `engine::cycle_state::run_cycle` and slice-8's `on_failure:` routing.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::ticket_task::{CycleResult, CycleRunner};
use crate::engine::context::CycleTrigger;
use crate::engine::cycle_state::{self, FailureMetadata};
use crate::engine::dispatch::DispatchTarget;
use crate::engine::outcome::{CycleKind, FailureKind};
use crate::engine::real_state_runner::RealStateRunner;
use crate::engine::state_runtime::CycleContext;
use crate::escalation::EscalationQueue;
use crate::events::{Event, EventWriter, FailureMarker, FailureMetaSer, now_rfc3339};
use crate::workflow::canonical::{RuleEntry, StateMachine};

pub struct RealCycleRunner {
    pub workflow: Arc<WorkflowConfig>,
    pub cfg: Arc<RokiConfig>,
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
                    meta: boot_failure(),
                    kind: CycleKind::Rule,
                };
            }
        };

        let (rule_entry, kind) = match target {
            DispatchTarget::CleanupShorthand => {
                let cycle_id = Uuid::new_v4();
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
            DispatchTarget::Cycle { kind, rule } => (rule, kind),
            DispatchTarget::NoMatch => {
                unreachable!("dispatcher only forwards matched targets")
            }
        };

        let cycle_id = Uuid::new_v4();
        let runner = build_runner(&self.cfg, admitted, cycle_id);
        let mut ctx = build_cycle_context(&self.cfg, admitted, cycle_id, kind, cycle_trigger, None);

        match cycle_state::run_cycle(&rule_entry.state_machine, &runner, &mut ctx).await {
            Ok(result) => {
                let _ = events.emit(&Event::CycleCompleted {
                    ts: now_rfc3339(),
                    cycle_id: cycle_id.to_string(),
                    cycle_kind: kind.as_str().to_string(),
                    iters: result.iterations,
                    terminal_id: Some(result.terminal_id.clone()),
                    outcome: Some(result.outcome.clone()),
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
                CycleResult::Completed {
                    kind,
                    iters: result.iterations,
                }
            }
            Err(meta) => {
                let decision = self
                    .handle_failed_cycle(
                        cycle_id,
                        &meta,
                        kind,
                        admitted,
                        &mut events,
                        cycle_trigger,
                    )
                    .await;
                match decision {
                    HandlerDecision::Succeeded => CycleResult::Completed {
                        kind: CycleKind::Failure,
                        iters: 0,
                    },
                    HandlerDecision::Unhandled => CycleResult::Failed {
                        meta: meta_to_legacy(&meta, cycle_id),
                        kind,
                    },
                }
            }
        }
    }
}

enum HandlerDecision {
    Succeeded,
    Unhandled,
}

impl RealCycleRunner {
    async fn handle_failed_cycle(
        &self,
        failed_cycle_id: Uuid,
        meta: &FailureMetadata,
        failed_kind: CycleKind,
        admitted: &AdmittedTicket,
        events: &mut EventWriter,
        cycle_trigger: CycleTrigger,
    ) -> HandlerDecision {
        // fr:06 trigger 1: a failure cycle that itself fails must not recurse.
        if failed_kind == CycleKind::Failure {
            self.escalation
                .push_cycle(
                    admitted.ticket.id.clone(),
                    failed_cycle_id,
                    meta.kind,
                    meta.state_id.clone(),
                    meta.error_text.clone(),
                )
                .await;
            return HandlerDecision::Unhandled;
        }

        let on_failures = self.workflow.on_failures_for(&admitted.ghq);
        let Some(handler) = crate::engine::on_failure::route(on_failures, meta) else {
            let _ = events.emit(&Event::FailureUnhandled {
                ts: now_rfc3339(),
                cycle_id: failed_cycle_id.to_string(),
                cycle_kind: failed_kind.as_str().to_string(),
                failure: FailureMetaSer::from_state_metadata(meta),
                marker: FailureMarker::None,
            });
            return HandlerDecision::Unhandled;
        };

        let handler_cycle_id = Uuid::new_v4();
        let handler_runner = build_runner(&self.cfg, admitted, handler_cycle_id);
        let mut handler_ctx = build_cycle_context(
            &self.cfg,
            admitted,
            handler_cycle_id,
            CycleKind::Failure,
            cycle_trigger,
            Some(meta),
        );

        match cycle_state::run_cycle(&handler.state_machine, &handler_runner, &mut handler_ctx)
            .await
        {
            Ok(result) => {
                let _ = events.emit(&Event::CycleCompleted {
                    ts: now_rfc3339(),
                    cycle_id: handler_cycle_id.to_string(),
                    cycle_kind: "failure".into(),
                    iters: result.iterations,
                    terminal_id: Some(result.terminal_id.clone()),
                    outcome: Some(result.outcome.clone()),
                });
                HandlerDecision::Succeeded
            }
            Err(handler_meta) => {
                self.escalation
                    .push_cycle(
                        admitted.ticket.id.clone(),
                        handler_cycle_id,
                        handler_meta.kind,
                        handler_meta.state_id.clone(),
                        handler_meta.error_text.clone(),
                    )
                    .await;
                HandlerDecision::Unhandled
            }
        }
    }
}

fn build_runner(cfg: &RokiConfig, admitted: &AdmittedTicket, cycle_id: Uuid) -> RealStateRunner {
    let session_root = cfg.paths.session_root.clone();
    let session_tempdir =
        session_root.join(crate::capture::sanitize_ticket_id(&admitted.ticket.id));
    RealStateRunner {
        default_cli: cfg.default_ai.cli.clone(),
        default_stall_seconds: cfg.default_ai.stall_seconds,
        ticket_id: admitted.ticket.id.clone(),
        ghq: admitted.ghq.clone(),
        session_root,
        session_tempdir,
        cycle_id,
    }
}

fn build_cycle_context(
    cfg: &RokiConfig,
    admitted: &AdmittedTicket,
    cycle_id: Uuid,
    kind: CycleKind,
    trigger: CycleTrigger,
    failure: Option<&FailureMetadata>,
) -> CycleContext {
    let mut globals = serde_json::Map::new();
    globals.insert(
        "ticket".into(),
        serde_json::json!({
            "id": admitted.ticket.id,
            "title": admitted.ticket.title,
            "body": admitted.ticket.body,
            "labels": admitted.ticket.labels,
            "assignee": admitted.ticket.assignee_id,
            "status": admitted.ticket.status,
        }),
    );
    globals.insert("repo".into(), serde_json::json!({ "ghq": admitted.ghq }));
    globals.insert(
        "cycle".into(),
        serde_json::json!({
            "id": cycle_id.to_string(),
            "kind": kind.as_str(),
            "trigger": trigger.as_str(),
            "iter": 0,
        }),
    );
    globals.insert(
        "config".into(),
        serde_json::json!({
            "max_iterations": cfg.engine.max_iterations,
        }),
    );
    if let Some(meta) = failure {
        globals.insert(
            "failure".into(),
            serde_json::json!({
                "kind": meta.kind.as_str(),
                "state_id": meta.state_id,
                "visit_n": meta.visit_n,
                "error_text": meta.error_text,
            }),
        );
    }
    CycleContext {
        globals,
        visits: std::collections::BTreeMap::new(),
        task_captures: std::collections::BTreeMap::new(),
        iter: 0,
        max_iterations: cfg.engine.max_iterations,
    }
}

/// Bridge `FailureMetadata` (slice-8 cycle driver) to a legacy-shaped tuple
/// the per-ticket task currently still expects in `CycleResult::Failed`.
/// The task only inspects this for evicting + escalation; the internals
/// are not user-visible after this point.
fn meta_to_legacy(meta: &FailureMetadata, failed_cycle_id: Uuid) -> LegacyFailureMeta {
    LegacyFailureMeta {
        failed_cycle_id,
        kind: meta.kind,
        state_id: meta.state_id.clone(),
        visit_n: meta.visit_n,
        error_text: meta.error_text.clone(),
    }
}

/// Slim replacement for the legacy `engine::outcome::FailureMeta`. Carries
/// only the fields the `CycleResult::Failed` consumer (per-ticket task
/// teardown path) reads.
#[derive(Debug, Clone)]
pub struct LegacyFailureMeta {
    pub failed_cycle_id: Uuid,
    pub kind: FailureKind,
    pub state_id: String,
    pub visit_n: u32,
    pub error_text: String,
}

fn boot_failure() -> LegacyFailureMeta {
    LegacyFailureMeta {
        failed_cycle_id: Uuid::nil(),
        kind: FailureKind::FsPoison,
        state_id: String::new(),
        visit_n: 0,
        error_text: "runner boot path".into(),
    }
}

// Re-export for the `_value` unused alias; silences `Value` import warning
// when feature flags hide all consumers.
#[allow(dead_code)]
fn _silence_unused_value() -> Value {
    Value::Null
}

#[allow(dead_code)]
fn _silence_unused_path() {
    let _: PathBuf = PathBuf::new();
    let _: &[RuleEntry] = &[];
    let _: Option<&StateMachine> = None;
}
