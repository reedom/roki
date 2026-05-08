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
use crate::rule;

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
    //    and rule no-match re-arm the loop. The first match locks the
    //    cycle and breaks out into the cycle-bound stage below.
    let (admitted, matched_rule) = loop {
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
            Ok(admitted) => match rule::first_match(&admitted, &workflow.rules) {
                Some(matched) => break (admitted, matched.clone()),
                None => {
                    tracing::info!(
                        ticket_id = %admitted.ticket.id,
                        "rule no-match; awaiting next webhook"
                    );
                    continue;
                }
            },
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

    let executor = crate::engine::CommandPhaseExecutor {
        default_cli: cfg.default_ai_command.cli.clone(),
        stall: crate::engine::phase::StallWindow::CommandDefault(
            cfg.default_ai_command.stall_seconds,
        ),
    };
    let outcome = crate::engine::run_cycle(
        &executor,
        &admitted,
        &matched_rule,
        &cfg.paths.session_root,
        &cfg,
    )
    .await?;

    // 8. Graceful shutdown.
    let _ = shutdown_tx.send(());
    let listener_result = listener_handle.await;

    // Cycle failures terminate the binary with exit 1.
    match outcome {
        crate::engine::CycleOutcome::Completed { .. } => match listener_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(SkeletonError::Webhook(err)),
            Err(join_err) => Err(SkeletonError::Webhook(WebhookError::BindFailed {
                addr: addr.to_string(),
                source: std::io::Error::other(join_err),
            })),
        },
        crate::engine::CycleOutcome::Failed { meta } => {
            tracing::error!(
                failure_kind = %meta.kind.as_str(),
                phase = %meta.phase.as_str(),
                iter = meta.iter,
                "cycle failed"
            );
            match listener_result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::error!(
                        error = %err,
                        "webhook listener errored after cycle failure"
                    );
                }
                Err(join_err) => {
                    tracing::error!(
                        error = %join_err,
                        "webhook listener task panicked after cycle failure"
                    );
                }
            }
            Err(SkeletonError::PhaseInfra(
                crate::error::PhaseInfraError::CycleFailed {
                    kind: meta.kind,
                    iter: meta.iter,
                },
            ))
        }
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
