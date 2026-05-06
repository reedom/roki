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

use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, oneshot};

use crate::admission;
use crate::capture;
use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::error::{SkeletonError, WebhookError};
use crate::linear::client::{LinearClient, MeId};
use crate::linear::ticket::NormalizedTicket;
use crate::linear::webhook::{self, WebhookState};
use crate::rule;
use crate::runner;

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

    // 7. Lock the cycle, drop the receiver, run the cycle. Setting the
    //    atomic before dropping the receiver guarantees every subsequent
    //    POST observes 503 (atomic == true OR `TrySendError::Closed`).
    cycle_started.store(true, Ordering::Release);
    drop(rx);

    let layout = capture::create(&cfg.paths.session_root, &admitted.ticket.id)?;
    let _outcome = runner::spawn(&matched_rule.run_cmd, &layout).await?;

    // Flush the runtime-owned capture handles before signalling shutdown.
    // The runner spawned `try_clone`'d handles for the child; the originals
    // are intact here and may carry buffered writes the runtime emitted
    // (none today, but the contract is documented in `runner` so future
    // pre/post phases inherit the flush).
    let mut layout = layout;
    let _ = layout.stdout.flush();
    let _ = layout.stderr.flush();
    drop(layout);

    // 8. Graceful shutdown. Signal axum to drain in-flight handlers, then
    //    join the listener task so a 503 reply for a post-cycle POST
    //    actually reaches the client before the binary exits.
    let _ = shutdown_tx.send(());
    match listener_handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(SkeletonError::Webhook(err)),
        Err(join_err) => Err(SkeletonError::Webhook(WebhookError::BindFailed {
            addr: addr.to_string(),
            source: std::io::Error::other(join_err),
        })),
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
