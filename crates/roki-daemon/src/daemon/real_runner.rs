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
use roki_store::models::{CycleKind as StoreCycleKind, CycleOutcome, NewCycle};

pub struct RealCycleRunner {
    pub workflow: Arc<WorkflowConfig>,
    pub cfg: Arc<RokiConfig>,
    pub escalation: Arc<EscalationQueue>,
    pub shutdown: crate::daemon::shutdown::ShutdownToken,
    pub inflight: Arc<crate::daemon::inflight::InflightRegistry>,
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
            Err(err) => {
                tracing::error!(
                    ticket_id = %admitted.ticket.id,
                    session_root = %self.cfg.paths.session_root.display(),
                    error = %err,
                    "failed to open per-ticket event writer at cycle boot"
                );
                return CycleResult::Failed {
                    meta: boot_failure(&err),
                    kind: CycleKind::Rule,
                };
            }
        };

        let (rule_entry, kind) = match target {
            DispatchTarget::CleanupShorthand => {
                let cycle_id = Uuid::new_v4();
                open_cycle_best_effort(
                    cycle_id,
                    &admitted.ticket.id,
                    CycleKind::Cleanup,
                    "cleanup-shorthand",
                );
                let cleanup_result = crate::engine::cleanup::delete_immediate(
                    &admitted.ticket.id,
                    &admitted.ghq,
                    &self.cfg.paths.session_root,
                    cycle_id,
                    &mut events,
                    &self.escalation,
                )
                .await;
                if cleanup_result.is_err() {
                    close_cycle_best_effort(cycle_id, CycleOutcome::Failure);
                    return CycleResult::CleanupFsError {
                        ticket_id: admitted.ticket.id.clone(),
                    };
                }
                close_cycle_best_effort(cycle_id, CycleOutcome::Success);
                return CycleResult::ShorthandDeleted;
            }
            DispatchTarget::Cycle { kind, rule } => (rule, kind),
            DispatchTarget::NoMatch => {
                unreachable!("dispatcher only forwards matched targets")
            }
        };

        let cycle_id = Uuid::new_v4();
        open_cycle_best_effort(cycle_id, &admitted.ticket.id, kind, "rule");
        let runner = build_runner(
            &self.cfg,
            admitted,
            cycle_id,
            self.shutdown.clone(),
            self.inflight.clone(),
        );
        let mut ctx = build_cycle_context(&self.cfg, admitted, cycle_id, kind, cycle_trigger, None);

        // Best-effort write of cycle.json at cycle start so the slice 9 HTTP
        // API (`GET /api/tickets/{id}/cycles`) has data to read mid-flight.
        // A failure here must not abort the cycle.
        let trigger_str = match cycle_trigger {
            CycleTrigger::Runtime => "runtime",
            CycleTrigger::ColdStart => "cold_start",
        };
        let states: Vec<String> = rule_entry.state_machine.states.keys().cloned().collect();
        let _ = crate::daemon::cycle_metadata::write_cycle_start(
            &self.cfg.paths.session_root,
            &admitted.ticket.id,
            cycle_id,
            kind,
            trigger_str,
            states,
        );

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
                close_cycle_best_effort(
                    cycle_id,
                    outcome_for_terminal(&result.outcome),
                );
                let _ = crate::daemon::cycle_metadata::write_cycle_end(
                    &self.cfg.paths.session_root,
                    &admitted.ticket.id,
                    cycle_id,
                    crate::daemon::cycle_metadata::CycleEndPayload {
                        terminal_id: Some(result.terminal_id.clone()),
                        failure_kind: None,
                        total_visits: result.iterations,
                    },
                );
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
                close_cycle_best_effort(cycle_id, CycleOutcome::Failure);
                let _ = crate::daemon::cycle_metadata::write_cycle_end(
                    &self.cfg.paths.session_root,
                    &admitted.ticket.id,
                    cycle_id,
                    crate::daemon::cycle_metadata::CycleEndPayload {
                        terminal_id: None,
                        failure_kind: Some(meta.kind),
                        total_visits: meta.visit_n,
                    },
                );
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
        open_cycle_best_effort(
            handler_cycle_id,
            &admitted.ticket.id,
            CycleKind::Failure,
            "failure-handler",
        );
        let handler_runner = build_runner(
            &self.cfg,
            admitted,
            handler_cycle_id,
            self.shutdown.clone(),
            self.inflight.clone(),
        );
        let mut handler_ctx = build_cycle_context(
            &self.cfg,
            admitted,
            handler_cycle_id,
            CycleKind::Failure,
            cycle_trigger,
            Some(meta),
        );

        // Failure-handler cycle is itself a real cycle that should surface in
        // `GET /api/tickets/{id}/cycles`. Best-effort write at start + end.
        let trigger_str = match cycle_trigger {
            CycleTrigger::Runtime => "runtime",
            CycleTrigger::ColdStart => "cold_start",
        };
        let handler_states: Vec<String> = handler.state_machine.states.keys().cloned().collect();
        let _ = crate::daemon::cycle_metadata::write_cycle_start(
            &self.cfg.paths.session_root,
            &admitted.ticket.id,
            handler_cycle_id,
            CycleKind::Failure,
            trigger_str,
            handler_states,
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
                close_cycle_best_effort(
                    handler_cycle_id,
                    outcome_for_terminal(&result.outcome),
                );
                let _ = crate::daemon::cycle_metadata::write_cycle_end(
                    &self.cfg.paths.session_root,
                    &admitted.ticket.id,
                    handler_cycle_id,
                    crate::daemon::cycle_metadata::CycleEndPayload {
                        terminal_id: Some(result.terminal_id.clone()),
                        failure_kind: None,
                        total_visits: result.iterations,
                    },
                );
                HandlerDecision::Succeeded
            }
            Err(handler_meta) => {
                close_cycle_best_effort(handler_cycle_id, CycleOutcome::Failure);
                let _ = crate::daemon::cycle_metadata::write_cycle_end(
                    &self.cfg.paths.session_root,
                    &admitted.ticket.id,
                    handler_cycle_id,
                    crate::daemon::cycle_metadata::CycleEndPayload {
                        terminal_id: None,
                        failure_kind: Some(handler_meta.kind),
                        total_visits: handler_meta.visit_n,
                    },
                );
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

fn build_runner(
    cfg: &RokiConfig,
    admitted: &AdmittedTicket,
    cycle_id: Uuid,
    shutdown: crate::daemon::shutdown::ShutdownToken,
    inflight: Arc<crate::daemon::inflight::InflightRegistry>,
) -> RealStateRunner {
    let session_root = cfg.paths.session_root.clone();
    let session_tempdir =
        session_root.join(crate::capture::sanitize_ticket_id(&admitted.ticket.id));
    RealStateRunner {
        default_cli: cfg.default.cli.clone(),
        default_stall_seconds: cfg.default.stall_seconds,
        ticket_id: admitted.ticket.id.clone(),
        ghq: admitted.ghq.clone(),
        session_root,
        session_tempdir,
        cycle_id,
        shutdown,
        inflight,
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
            "session_root": cfg.paths.session_root.to_string_lossy(),
        }),
    );
    if let Some(port) = cfg.api.port {
        let bind = if cfg.api.bind.is_empty() {
            "127.0.0.1".to_string()
        } else {
            cfg.api.bind.clone()
        };
        // Land as namespace `api.url` so the scalar flattener emits
        // ROKI_API_URL (the documented env name) rather than
        // ROKI_CONFIG_API_URL.
        globals.insert(
            "api".into(),
            serde_json::json!({ "url": format!("http://{bind}:{port}") }),
        );
    }
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
        cycle_id: cycle_id.to_string(),
    }
}

/// Map the daemon-side `CycleKind` enum to the store-side equivalent.
fn store_cycle_kind(kind: CycleKind) -> StoreCycleKind {
    match kind {
        CycleKind::Rule => StoreCycleKind::Rule,
        CycleKind::Cleanup => StoreCycleKind::Cleanup,
        CycleKind::Failure => StoreCycleKind::Failure,
    }
}

/// Map a terminal outcome string (the workflow author's free-form label) onto
/// the store's bounded `CycleOutcome` enum. The two outcomes that are not
/// catch-alls are `no_action` (rule-cycle saw "nothing to do") and `cancelled`
/// (shutdown interruption); anything else is treated as `success` at the
/// "did the cycle reach a terminal" level. `Failure` is set on the error path
/// directly and never flows through this function.
fn outcome_for_terminal(outcome: &str) -> CycleOutcome {
    match outcome {
        "no_action" => CycleOutcome::NoAction,
        "cancelled" => CycleOutcome::Cancelled,
        _ => CycleOutcome::Success,
    }
}

/// Open a new row in the `cycles` table for this spawn. Best-effort: errors
/// are warn-logged in `with_store` and never propagated. The daemon owns the
/// UUID, so the routing-key UUID on emitted events lines up with the row id.
fn open_cycle_best_effort(cycle_id: Uuid, ticket_id: &str, kind: CycleKind, entry_name: &str) {
    let new_cycle = NewCycle {
        id: cycle_id.to_string(),
        ticket_id: ticket_id.to_string(),
        kind: store_cycle_kind(kind),
        entry_name: entry_name.to_string(),
        started_at: crate::store_handle::now_unix_millis(),
    };
    crate::store_handle::with_store("open_cycle", |store| {
        store.open_cycle(new_cycle.clone()).map(|_| ())
    });
}

/// Stamp `outcome` + `ended_at` on the cycle row. Best-effort.
fn close_cycle_best_effort(cycle_id: Uuid, outcome: CycleOutcome) {
    let cid = cycle_id.to_string();
    let ended_at = crate::store_handle::now_unix_millis();
    crate::store_handle::with_store("close_cycle", move |store| {
        store.close_cycle(&cid, outcome, ended_at)
    });
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

fn boot_failure(err: &std::io::Error) -> LegacyFailureMeta {
    LegacyFailureMeta {
        failed_cycle_id: Uuid::nil(),
        kind: FailureKind::FsPoison,
        state_id: String::new(),
        visit_n: 0,
        error_text: format!("event writer open: {err}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::ticket::NormalizedTicket;

    fn admitted_ticket() -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "ENG-1".into(),
                None,
                "Backlog".into(),
                vec![],
                "t".into(),
                "b".into(),
            ),
            ghq: "github.com/x/y".into(),
        }
    }

    #[test]
    fn build_cycle_context_exports_session_root_into_globals_config() {
        let cfg = RokiConfig::test_default(std::path::Path::new("/tmp/sess-x"));
        let admitted = admitted_ticket();
        let cx = build_cycle_context(
            &cfg,
            &admitted,
            Uuid::nil(),
            CycleKind::Rule,
            CycleTrigger::Runtime,
            None,
        );
        let cfg_obj = cx
            .globals
            .get("config")
            .and_then(|v| v.as_object())
            .expect("config namespace present");
        assert_eq!(
            cfg_obj.get("session_root").and_then(|v| v.as_str()),
            Some("/tmp/sess-x")
        );
    }

    #[test]
    fn build_cycle_context_exports_api_url_when_port_set() {
        let mut cfg = RokiConfig::test_default(std::path::Path::new("/tmp/sess-x"));
        cfg.api.port = Some(7777);
        // bind defaults to 127.0.0.1 in test_default; verify the synthesized URL.
        let admitted = admitted_ticket();
        let cx = build_cycle_context(
            &cfg,
            &admitted,
            Uuid::nil(),
            CycleKind::Rule,
            CycleTrigger::Runtime,
            None,
        );
        let url = cx
            .globals
            .get("api")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
            .expect("api.url present");
        assert_eq!(url, "http://127.0.0.1:7777");
    }

    #[test]
    fn build_cycle_context_omits_api_url_when_port_unset() {
        let cfg = RokiConfig::test_default(std::path::Path::new("/tmp/sess-x"));
        assert!(cfg.api.port.is_none());
        let admitted = admitted_ticket();
        let cx = build_cycle_context(
            &cfg,
            &admitted,
            Uuid::nil(),
            CycleKind::Rule,
            CycleTrigger::Runtime,
            None,
        );
        assert!(cx.globals.get("api").is_none());
    }
}
