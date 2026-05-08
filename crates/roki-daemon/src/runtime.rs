// Walking-skeleton tasks land in dependency order: this orchestrator (task
// 5.1) precedes the cli (5.2) and main wiring (5.3) that will call
// `runtime::run`. Until those land, the public entry is exercised only by
// the unit test below, which triggers `dead_code` for the leaf API. Allow
// it module-locally instead of leaking the relaxation crate-wide, matching
// the pattern in `admission`, `capture`, `rule`, `runner`, and the config
// loaders.
#![allow(dead_code)]

//! Runtime orchestrator for the walking-skeleton daemon.
//!
//! Wires loaded config → bound webhook listener → admission / rule pipeline
//! → single cycle execution → graceful shutdown. Per design `runtime` block,
//! the runtime owns the `Option<MeId>` plus the `Sender` / `Receiver` /
//! `cycle_started` triple — no mutex, no swap, no placeholder window.
//!
//! Failure classes:
//!
//! - **Startup-bound** (`RokiConfig`, `WorkflowConfig`, `me` resolve, listener
//!   bind) — abort before the listener accepts traffic; `ExitCode::FAILURE`.
//! - **Cycle-bound** (capture, runner) — short-circuit out of the loop;
//!   `ExitCode::FAILURE`.
//! - **No-cycle outcomes** (admission rejection, rule no-match) — info-log
//!   and `continue`; the listener stays open for the next POST.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, oneshot};

use crate::admission;
use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::error::{SkeletonError, WebhookError};
use crate::linear::client::{LinearClient, MeId};
use crate::linear::ticket::NormalizedTicket;
use crate::linear::webhook::{self, WebhookState};

/// Carries the resolved dispatch target through to the cycle call site.
enum DispatchedEntry {
    Rule(crate::config::workflow::Rule),
    Cleanup(crate::config::workflow::Cleanup),
    Shorthand,
}

/// Run the skeleton pipeline.
///
/// Returns `ExitCode::SUCCESS` on a clean cycle (regardless of the
/// subprocess exit code, per Req 8.2) and `ExitCode::FAILURE` on any
/// internal error in the startup-bound or cycle-bound classes.
pub async fn run(config_path: &Path) -> ExitCode {
    match run_inner(config_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "skeleton runtime exited with internal error");
            ExitCode::FAILURE
        }
    }
}

/// Internal pipeline. Separated from [`run`] so unit tests can match on the
/// typed [`SkeletonError`] surface without parsing an `ExitCode`.
pub(crate) async fn run_inner(config_path: &Path) -> Result<(), SkeletonError> {
    // 1. Load roki.toml.
    let cfg = RokiConfig::load(config_path)?;

    // 2. Load WORKFLOW.toml.
    let workflow = WorkflowConfig::load(&cfg.paths.workflow)?;

    // 3. Resolve `me` only when admission says "me"; any other value is a
    //    literal Linear user id compared verbatim by `admission::accept`.
    let me = if workflow.admission.assignee == "me" {
        let client = LinearClient::new(cfg.linear.token.clone());
        Some(client.resolve_viewer().await?)
    } else {
        None
    };

    // 4. Channel + atomic. Channel capacity 1; atomic is the cycle-started
    //    signal the webhook handler reads with `Acquire` and the runtime
    //    sets exactly once with `Release` per design State Management.
    let (tx, mut rx) = mpsc::channel::<NormalizedTicket>(1);
    let cycle_started = Arc::new(AtomicBool::new(false));

    // 5. Bind webhook listener. Bind failure is startup-bound (Req 3.1) and
    //    aborts before any POST is accepted.
    let bind_ip = IpAddr::from_str(&cfg.linear_webhook.bind).map_err(|err| {
        SkeletonError::Webhook(WebhookError::BindFailed {
            addr: cfg.linear_webhook.bind.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, err),
        })
    })?;
    let addr = SocketAddr::from((bind_ip, cfg.linear_webhook.port));

    let state = WebhookState {
        sender: Arc::new(tx),
        cycle_started: cycle_started.clone(),
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let listener_handle = tokio::spawn(webhook::bind_and_serve(addr, state, async move {
        let _ = shutdown_rx.await;
    }));

    // 6. Pipeline loop. Each iteration drains one POST; admission rejection
    //    and dispatch no-match re-arm the loop. The first match locks the
    //    cycle and breaks out into the cycle-bound stage below.
    let (admitted, _cycle_kind, dispatched) = loop {
        let Some(ticket) = rx.recv().await else {
            // Sender side dropped without ever delivering a ticket. The
            // listener task either failed to bind or exited prematurely;
            // surface the failure as a bind error so the operator sees the
            // listener address in the log line.
            let _ = shutdown_tx.send(());
            return match listener_handle.await {
                Ok(Ok(())) => Err(SkeletonError::Webhook(WebhookError::BindFailed {
                    addr: addr.to_string(),
                    source: std::io::Error::other("webhook channel closed before cycle"),
                })),
                Ok(Err(err)) => Err(SkeletonError::Webhook(err)),
                Err(join_err) => Err(SkeletonError::Webhook(WebhookError::BindFailed {
                    addr: addr.to_string(),
                    source: std::io::Error::other(join_err),
                })),
            };
        };

        // `admission::accept` requires `&MeId`. When admission compares
        // against a literal id, the field is unused; pass an empty value.
        let me_ref = me.clone().unwrap_or_else(|| MeId(String::new()));
        match admission::accept(&ticket, &workflow, &me_ref) {
            Ok(admitted) => {
                use crate::engine::dispatch::{evaluate, DispatchMode, DispatchTarget};
                match evaluate(&admitted, &workflow, DispatchMode::Default) {
                    DispatchTarget::Cycle { kind, rule: Some(r), .. } => {
                        break (admitted, kind, DispatchedEntry::Rule(r.clone()));
                    }
                    DispatchTarget::Cycle { kind, cleanup: Some(c), .. } => {
                        break (admitted, kind, DispatchedEntry::Cleanup(c.clone()));
                    }
                    DispatchTarget::CleanupShorthand => {
                        break (
                            admitted,
                            crate::engine::outcome::CycleKind::Cleanup,
                            DispatchedEntry::Shorthand,
                        );
                    }
                    DispatchTarget::NoMatch => {
                        tracing::info!(
                            ticket_id = %admitted.ticket.id,
                            "no dispatch match; awaiting next webhook"
                        );
                        continue;
                    }
                    DispatchTarget::Cycle { rule: None, cleanup: None, .. } => unreachable!(
                        "dispatch::evaluate returned Cycle with neither rule nor cleanup"
                    ),
                }
            }
            Err(err) => {
                tracing::info!(
                    ticket_id = %ticket.id,
                    reason = %err,
                    "admission rejected; awaiting next webhook"
                );
                continue;
            }
        }
    };

    // 7. Lock the cycle, drop the receiver, dispatch into the engine.
    cycle_started.store(true, Ordering::Release);
    drop(rx);

    let mut events = crate::events::EventWriter::open(&cfg.paths.session_root, &admitted.ticket.id)
        .map_err(|e| {
            SkeletonError::Capture(crate::error::CaptureError::OpenFile {
                path: crate::events::events_path(&cfg.paths.session_root, &admitted.ticket.id),
                source: e,
            })
        })?;

    let executor = crate::engine::CommandPhaseExecutor {
        default_cli: cfg.default_ai_command.cli.clone(),
        stall: crate::engine::phase::StallWindow::CommandDefault(
            cfg.default_ai_command.stall_seconds,
        ),
    };

    enum CycleOutcomeOrShortcircuit {
        Cycle {
            kind: crate::engine::outcome::CycleKind,
            outcome: crate::engine::CycleOutcome,
        },
        Shorthand,
    }

    let cycle_outcome_result: CycleOutcomeOrShortcircuit = match dispatched {
        DispatchedEntry::Rule(rule) => {
            let outcome = crate::engine::run_cycle(
                &executor,
                &admitted,
                &rule,
                &cfg.paths.session_root,
                &cfg,
                crate::engine::outcome::CycleKind::Rule,
                None,
            )
            .await?;
            CycleOutcomeOrShortcircuit::Cycle {
                kind: crate::engine::outcome::CycleKind::Rule,
                outcome,
            }
        }
        DispatchedEntry::Cleanup(cleanup) => {
            let rule_view = cleanup_to_rule(&cleanup);
            let outcome = crate::engine::run_cycle(
                &executor,
                &admitted,
                &rule_view,
                &cfg.paths.session_root,
                &cfg,
                crate::engine::outcome::CycleKind::Cleanup,
                None,
            )
            .await?;
            CycleOutcomeOrShortcircuit::Cycle {
                kind: crate::engine::outcome::CycleKind::Cleanup,
                outcome,
            }
        }
        DispatchedEntry::Shorthand => {
            crate::engine::cleanup::delete_immediate(
                &admitted.ticket.id,
                &cfg.paths.session_root,
                &mut events,
            )
            .map_err(|e| {
                SkeletonError::Capture(crate::error::CaptureError::Write {
                    path: events.path().to_path_buf(),
                    source: std::io::Error::other(e),
                })
            })?;
            CycleOutcomeOrShortcircuit::Shorthand
        }
    };

    // 8. Handle the cycle outcome. The failure handler must run before the
    //    listener is shut down because it may spawn a Failure cycle.
    let final_result: Result<(), SkeletonError> = match cycle_outcome_result {
        CycleOutcomeOrShortcircuit::Shorthand => {
            // delete_immediate already emitted both events. Exit 0.
            Ok(())
        }
        CycleOutcomeOrShortcircuit::Cycle { kind, outcome } => match outcome {
            crate::engine::CycleOutcome::Completed { iters, cycle_id } => {
                let _ = events.emit(&crate::events::Event::CycleCompleted {
                    ts: crate::events::now_rfc3339(),
                    cycle_id: cycle_id.to_string(),
                    cycle_kind: kind.as_str().to_string(),
                    iters,
                    outcome: None,
                });
                if kind == crate::engine::outcome::CycleKind::Cleanup {
                    crate::engine::cleanup::post_cycle_delete(
                        &admitted.ticket.id,
                        &cfg.paths.session_root,
                        cycle_id,
                        &mut events,
                    )
                    .map_err(|e| {
                        SkeletonError::Capture(crate::error::CaptureError::Write {
                            path: events.path().to_path_buf(),
                            source: std::io::Error::other(e),
                        })
                    })?;
                }
                Ok(())
            }
            crate::engine::CycleOutcome::Failed { meta } => {
                tracing::error!(
                    failure_kind = %meta.kind.as_str(),
                    phase = %meta.phase.as_str(),
                    iter = meta.iter,
                    "cycle failed"
                );
                let decision = handle_failed_cycle(
                    &meta,
                    kind,
                    &workflow,
                    &executor,
                    &admitted,
                    &cfg,
                    &mut events,
                )
                .await;
                match decision {
                    FailureDecision::HandlerSucceeded => Ok(()),
                    FailureDecision::Unhandled => {
                        Err(SkeletonError::PhaseInfra(
                            crate::error::PhaseInfraError::CycleFailed {
                                kind: meta.kind,
                                iter: meta.iter,
                            },
                        ))
                    }
                }
            }
        },
    };

    // 9. Graceful shutdown — runs exactly once on every exit path.
    let _ = shutdown_tx.send(());
    let listener_result = listener_handle.await;

    // Merge listener errors into the final result.
    match final_result {
        Ok(()) => match listener_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(SkeletonError::Webhook(err)),
            Err(join_err) => Err(SkeletonError::Webhook(WebhookError::BindFailed {
                addr: addr.to_string(),
                source: std::io::Error::other(join_err),
            })),
        },
        Err(cycle_err) => {
            match listener_result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::error!(error = %err, "webhook listener errored after cycle failure");
                }
                Err(join_err) => {
                    tracing::error!(
                        error = %join_err,
                        "webhook listener task panicked after cycle failure"
                    );
                }
            }
            Err(cycle_err)
        }
    }
}

enum FailureDecision {
    HandlerSucceeded,
    Unhandled,
}

async fn handle_failed_cycle(
    meta: &crate::engine::outcome::FailureMeta,
    failed_cycle_kind: crate::engine::outcome::CycleKind,
    workflow: &crate::config::workflow::WorkflowConfig,
    executor: &crate::engine::CommandPhaseExecutor,
    admitted: &crate::admission::AdmittedTicket,
    cfg: &crate::config::roki::RokiConfig,
    events: &mut crate::events::EventWriter,
) -> FailureDecision {
    use crate::engine::outcome::CycleKind;
    use crate::events::{Event, FailureMarker, FailureMetaSer, now_rfc3339};

    // Recursion bound: a failure cycle that itself fails must not recurse.
    if failed_cycle_kind == CycleKind::Failure {
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: meta.failed_cycle_id.to_string(),
            cycle_kind: "failure".into(),
            failure: FailureMetaSer::from_meta(meta),
            marker: FailureMarker::RecursionBound,
        });
        return FailureDecision::Unhandled;
    }

    // First-match against [[on_failure]].
    let Some(handler) = crate::engine::on_failure::route(&workflow.on_failures, meta) else {
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: meta.failed_cycle_id.to_string(),
            cycle_kind: failed_cycle_kind.as_str().to_string(),
            failure: FailureMetaSer::from_meta(meta),
            marker: FailureMarker::None,
        });
        return FailureDecision::Unhandled;
    };

    // Convert OnFailure -> Rule shape for run_cycle.
    let handler_rule = on_failure_to_rule(handler);
    let handler_outcome = match crate::engine::run_cycle(
        executor,
        admitted,
        &handler_rule,
        &cfg.paths.session_root,
        cfg,
        CycleKind::Failure,
        Some(meta.clone()),
    )
    .await
    {
        Ok(o) => o,
        Err(infra) => {
            tracing::error!(?infra, "handler cycle infra error");
            let _ = events.emit(&Event::FailureUnhandled {
                ts: now_rfc3339(),
                cycle_id: meta.failed_cycle_id.to_string(),
                cycle_kind: "failure".into(),
                failure: FailureMetaSer::from_meta(meta),
                marker: FailureMarker::RecursionBound,
            });
            return FailureDecision::Unhandled;
        }
    };

    match handler_outcome {
        crate::engine::CycleOutcome::Completed { iters, cycle_id } => {
            let _ = events.emit(&Event::CycleCompleted {
                ts: now_rfc3339(),
                cycle_id: cycle_id.to_string(),
                cycle_kind: "failure".into(),
                iters,
                outcome: None,
            });
            FailureDecision::HandlerSucceeded
        }
        crate::engine::CycleOutcome::Failed { meta: handler_meta } => {
            let _ = events.emit(&Event::FailureUnhandled {
                ts: now_rfc3339(),
                cycle_id: handler_meta.failed_cycle_id.to_string(),
                cycle_kind: "failure".into(),
                failure: FailureMetaSer::from_meta(&handler_meta),
                marker: FailureMarker::RecursionBound,
            });
            FailureDecision::Unhandled
        }
    }
}

fn on_failure_to_rule(
    h: &crate::engine::on_failure::OnFailure,
) -> crate::config::workflow::Rule {
    crate::config::workflow::Rule {
        when_status: String::new(),
        when_labels_has_all: vec![],
        pre: h.pre.clone(),
        run: h.run.clone(),
        post: h.post.clone(),
    }
}

/// Convert a `Cleanup` entry into a `Rule` view for `run_cycle`.
/// Only called for non-shorthand cleanup entries; `run` is present by
/// parser invariant (`WorkflowError::CleanupMissingRun` guards the load).
fn cleanup_to_rule(c: &crate::config::workflow::Cleanup) -> crate::config::workflow::Rule {
    crate::config::workflow::Rule {
        when_status: c.when_status.clone().unwrap_or_default(),
        when_labels_has_all: c.when_labels_has_all.clone(),
        pre: c.pre.clone(),
        run: c.run.clone().expect("non-shorthand cleanup has run"),
        post: c.post.clone(),
    }
}

#[cfg(test)]
mod tests {
    //! Startup-bound failure tests. The cycle-bound and happy-path coverage
    //! lives in the end-to-end smoke test (task 6.1) per design Testing
    //! Strategy: the orchestrator's full pipeline is exercised via the
    //! binary-as-subprocess path so the wire-level webhook → cycle →
    //! shutdown behavior is acceptance-tested rather than mocked.

    use super::*;
    use crate::error::{RokiConfigError, SkeletonError};
    use tempfile::TempDir;

    #[tokio::test]
    async fn missing_config_returns_config_error() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("does-not-exist.toml");

        match run_inner(&nonexistent).await {
            Err(SkeletonError::Config(RokiConfigError::MissingFile { path })) => {
                assert_eq!(path, nonexistent);
            }
            other => panic!("expected SkeletonError::Config(MissingFile), got {other:?}"),
        }
    }
}
