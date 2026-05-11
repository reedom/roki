// Walking-skeleton tasks land in dependency order: this orchestrator (task
// 5.1) precedes the cli (5.2) and main wiring (5.3) that will call
// `runtime::run`. Until those land, the public entry is exercised only by
// the unit test below, which triggers `dead_code` for the leaf API. Allow
// it module-locally instead of leaking the relaxation crate-wide, matching
// the pattern in `admission`, `capture`, `rule`, `runner`, and the config
// loaders.
#![allow(dead_code)]

//! Persistent-daemon runtime orchestrator (slice 5).
//!
//! Boots config + workflow + listener + dispatcher + per-ticket task
//! registry, traps SIGINT/SIGTERM via `tokio::signal::unix`, and drains
//! in-flight ticket tasks within `cfg.engine.shutdown_window_seconds`.
//!
//! Failure classes:
//!
//! - **Startup-bound** (`RokiConfig`, `WorkflowConfig`, `me` resolve, listener
//!   bind) — abort before the listener accepts traffic; `ExitCode::FAILURE`.
//! - **Shutdown-window-exceeded** — at least one ticket task did not drain
//!   within the configured window; emit `Event::ShutdownWindowExceeded`
//!   and return `Err(SkeletonError::ShutdownWindowExceeded)` so the binary
//!   exits with `ExitCode::FAILURE`.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;

pub use crate::engine::dispatch::DispatchMode;

use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::cache::DiffCache;
use crate::daemon::dispatcher::Dispatcher;
use crate::daemon::real_runner::RealCycleRunner;
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::ticket_task::DispatchMsg;
use crate::error::{SkeletonError, WebhookError};
use crate::events::{Event, EventWriter, ShutdownSignal, now_rfc3339};
use crate::linear::client::LinearClient;
use crate::linear::webhook::{self, WebhookState};

/// Run the persistent daemon.
///
/// Returns `ExitCode::SUCCESS` on a clean drain (every in-flight ticket
/// task exited within `shutdown_window_seconds`), and `ExitCode::FAILURE`
/// on any startup-bound error or on a shutdown-window timeout.
pub async fn run(config_path: &Path, mode: DispatchMode) -> ExitCode {
    match run_inner(config_path, mode).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "daemon runtime exited with internal error");
            ExitCode::FAILURE
        }
    }
}

/// Internal pipeline. Separated from [`run`] so unit tests can match on the
/// typed [`SkeletonError`] surface without parsing an `ExitCode`.
pub(crate) async fn run_inner(config_path: &Path, mode: DispatchMode) -> Result<(), SkeletonError> {
    // 1. Load roki.toml.
    let cfg = RokiConfig::load(config_path)?;

    // 2. Load WORKFLOW.yaml.
    let workflow = WorkflowConfig::load(&cfg.paths.workflow)?;

    // Shared rate-limit state — both the viewer-resolve client below and
    // the cold-start GraphQL enumerate client share this atom so a 429
    // observed by either path defers the other.
    let rate_limit = Arc::new(crate::linear::rate_limit::RateLimitState::new());

    // 3. Resolve `me` only when admission says "me"; any other value is a
    //    literal Linear user id compared verbatim by `admission::accept`.
    let me = if workflow.admission.assignee == "me" {
        let client = LinearClient::new(cfg.linear.token.clone(), rate_limit.clone());
        Some(client.resolve_viewer().await?)
    } else {
        None
    };

    let cfg = Arc::new(cfg);
    let workflow = Arc::new(workflow);

    // fr:08 Tier 3 in-memory ring buffer for the observability HTTP API.
    // Defaults to 1000 when `log.ring_size` is absent in roki.toml.
    let ring_capacity = cfg.log.ring_size.unwrap_or(1000) as usize;
    let ring = crate::observability::EventRing::new(ring_capacity);
    // Install the ring globally so every `EventWriter::emit` mirrors into it
    // (see events.rs::EventWriter::emit). Must happen before the daemon-scoped
    // writer is opened and the first `daemon_started` event is emitted so the
    // boot sequence shows up in `/api/events`.
    crate::observability::set_global_ring(ring.clone());

    // 4. Open the daemon-scoped event log.
    let daemon_events_writer =
        EventWriter::open(&cfg.paths.session_root, "_daemon").map_err(|e| {
            SkeletonError::Capture(crate::error::CaptureError::OpenFile {
                path: crate::events::events_path(&cfg.paths.session_root, "_daemon"),
                source: e,
            })
        })?;
    let daemon_events = Arc::new(Mutex::new(daemon_events_writer));

    // 4b. Build escalation queue (fr:06 §Escalation queue) — wired before
    //     DaemonStarted so any startup-bound failure has a receiver.
    let escalation = crate::escalation::EscalationQueue::new(
        cfg.escalation.queue_size as usize,
        daemon_events.clone(),
    );

    // 5. Emit DaemonStarted.
    {
        let mut w = daemon_events.lock().await;
        let _ = w.emit(&Event::DaemonStarted {
            ts: now_rfc3339(),
            config_path: config_path.display().to_string(),
            schema_version: 1,
        });
    }

    // 6. Bind listener (capacity 64).
    let bind_ip = IpAddr::from_str(&cfg.linear_webhook.bind).map_err(|err| {
        SkeletonError::Webhook(WebhookError::BindFailed {
            addr: cfg.linear_webhook.bind.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, err),
        })
    })?;
    let addr = SocketAddr::from((bind_ip, cfg.linear_webhook.port));

    // fr:09 polling-fallback outage signal: bumped on every successful
    // webhook 202; PollingTracker reads this to gate outage ticks.
    let last_webhook_success = Arc::new(std::sync::atomic::AtomicI64::new(0));

    let (tx, rx) = mpsc::channel(64);
    let state = WebhookState {
        sender: Arc::new(tx),
        last_webhook_success: Some(last_webhook_success.clone()),
    };

    let shutdown = ShutdownToken::new();

    // The listener binds *before* cold start runs so Linear webhooks
    // arriving during the cold-start window get a deterministic
    // `503 cold_start_in_progress` reply rather than a TCP connect
    // refusal. The gate opens immediately before `daemon_ready`.
    let ready_gate = crate::linear::webhook::ReadyGate::new();

    let listener_shutdown = shutdown.clone();
    let listener_handle = tokio::spawn(webhook::bind_and_serve(
        addr,
        state,
        ready_gate.clone(),
        async move {
            listener_shutdown.wait().await;
        },
    ));

    // 7. Build runner. Slice 8: the runner constructs a `RealStateRunner`
    //    per cycle internally, sourced from `cfg.default_ai`.
    let runner = Arc::new(RealCycleRunner {
        workflow: workflow.clone(),
        cfg: cfg.clone(),
        escalation: escalation.clone(),
    });

    // 9. Build cache + dispatcher.
    let cache = Arc::new(DiffCache::new());
    let dispatcher = Arc::new(Dispatcher::new(
        cache.clone(),
        workflow.clone(),
        cfg.clone(),
        me.clone(),
        mode,
        shutdown.clone(),
        runner,
        daemon_events.clone(),
        escalation.clone(),
    ));

    // 10. Cold start (fr:07): paginated GraphQL enumerate -> cache
    //     populate -> dispatch with ColdStart trigger -> orphan
    //     reconcile. Runs while `ready_gate` is still closed so any
    //     webhooks arriving during the window observe 503
    //     `cold_start_in_progress`.
    let graphql = Arc::new(crate::linear::graphql::LinearGraphqlClient::with_writer(
        cfg.linear.token.clone(),
        rate_limit.clone(),
        Some(daemon_events.clone()),
    ));

    {
        let mut w = daemon_events.lock().await;
        let _ = w.emit(&Event::ColdStartBegan {
            ts: now_rfc3339(),
            roki_toml_path: config_path.display().to_string(),
            workflow_toml_path: cfg.paths.workflow.display().to_string(),
        });
    }

    let cold_start = crate::daemon::cold_start::ColdStart {
        cfg: cfg.clone(),
        workflow: workflow.clone(),
        me: me.clone(),
        cache: cache.clone(),
        dispatcher: dispatcher.clone(),
        graphql,
        mode,
        escalation: escalation.clone(),
    };
    // Pass the shared writer Arc directly so cold_start (and the
    // GraphQL client it drives) can take and drop the lock around each
    // emit individually. Holding the lock across `cold_start.run` would
    // deadlock with `LinearGraphqlClient::enumerate`'s 429 path, which
    // re-locks the same writer to emit `linear_backoff_applied`.
    let report = cold_start.run(daemon_events.clone()).await;

    {
        let mut w = daemon_events.lock().await;
        let _ = w.emit(&Event::ColdStartCompleted {
            ts: now_rfc3339(),
            enumerated: report.enumerated,
            admitted: report.admitted,
            cycles_spawned: report.cycles_spawned,
            orphans_deleted: report.orphans_deleted,
            enum_partial: report.enum_partial,
            partial_reason: report.partial_reason.clone(),
            partial_error_text: report.partial_error_text.clone(),
        });
    }

    // 11. Open the gate, then DaemonReady. Order matters: the
    //     `daemon_ready` event is the contract the operator and tests
    //     wait on, and webhooks must already be admitted by the time
    //     that event lands on disk.
    ready_gate.open();
    {
        let mut w = daemon_events.lock().await;
        let _ = w.emit(&Event::DaemonReady {
            ts: now_rfc3339(),
            webhook_bind_addr: addr.to_string(),
        });
    }

    // 11a. Spawn PollingTracker (fr:09 §2.4). Outage gating reads
    //      `last_webhook_success`; nudges arrive via the API's
    //      `POST /admin/refresh-now` handler. The on-tick callback emits
    //      `polling_tick`; the full enumerate-on-tick wiring is deferred
    //      to slice 10 per design spec §9 Documented divergence.
    let polling_cadence =
        std::time::Duration::from_secs(cfg.linear.polling.cadence_seconds as u64);
    let daemon_writer_for_tick = daemon_events.clone();
    let nudge_handle = crate::linear::polling::PollingTracker::spawn(
        polling_cadence,
        rate_limit.clone(),
        last_webhook_success.clone(),
        Box::new(move |reason| {
            let writer = daemon_writer_for_tick.clone();
            let trigger = match reason {
                crate::linear::polling::TickReason::Outage => "outage",
                crate::linear::polling::TickReason::Nudge => "nudge",
            };
            tokio::spawn(async move {
                let mut w = writer.lock().await;
                let _ = w.emit(&Event::PollingTick {
                    ts: now_rfc3339(),
                    trigger: trigger.into(),
                    status_set: Vec::new(),
                    enumerated: 0,
                    admitted: 0,
                });
            });
        }),
    );

    // 11b. Spawn the observability HTTP API (fr:10) when `[api].port`
    //      is set. Bind failure is logged as `api_bind_failed` rather
    //      than aborting the daemon — the operator can still observe
    //      cycles via the file-backed event log.
    if cfg.api.port.is_some() {
        let api_state = Arc::new(crate::api::ApiState {
            cache: cache.clone(),
            workflow: workflow.clone(),
            cfg: cfg.clone(),
            escalation: escalation.clone(),
            ring: ring.clone(),
            nudge: nudge_handle.clone(),
            request_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            boot_time: time::OffsetDateTime::now_utc(),
            daemon_writer: daemon_events.clone(),
        });
        let writer_for_log = daemon_events.clone();
        tokio::spawn(async move {
            match crate::api::serve(api_state).await {
                Ok(()) => {}
                Err(crate::api::ApiBindError::Bind { bind, port, source }) => {
                    let mut w = writer_for_log.lock().await;
                    let _ = w.emit(&Event::ApiBindFailed {
                        ts: now_rfc3339(),
                        bind,
                        port,
                        error: source.to_string(),
                    });
                }
            }
        });
    } else {
        let mut w = daemon_events.lock().await;
        let _ = w.emit(&Event::ApiDisabled {
            ts: now_rfc3339(),
        });
    }
    // Keep the nudge handle alive until daemon shutdown; without this the
    // PollingTracker's `recv` returns None and the task exits.
    let _nudge_handle_keepalive = nudge_handle;

    // 12. Spawn dispatcher drain.
    let dispatcher_drain = dispatcher.clone();
    let drain_handle = tokio::spawn(async move {
        dispatcher_drain.drain(rx).await;
    });

    // 13. Spawn signal trap.
    let signal_shutdown = shutdown.clone();
    let signal_cache = cache.clone();
    let signal_events = daemon_events.clone();
    let signal_handle = tokio::spawn(async move {
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "failed to install SIGINT handler");
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "failed to install SIGTERM handler");
                return;
            }
        };

        let signal_kind = tokio::select! {
            _ = sigint.recv() => ShutdownSignal::Sigint,
            _ = sigterm.recv() => ShutdownSignal::Sigterm,
        };

        let in_flight = signal_cache.in_flight_count().await;
        {
            let mut w = signal_events.lock().await;
            let _ = w.emit(&Event::DaemonShutdownBegan {
                ts: now_rfc3339(),
                signal: signal_kind,
                in_flight,
            });
        }
        signal_shutdown.fire();
    });

    // 14. Block on shutdown.
    shutdown.wait().await;

    // 15. Wait for the listener and dispatcher drain to wind down. The
    //     listener observes the shared `ShutdownToken` via the future
    //     handed to `bind_and_serve`. The dispatcher's drain loop exits
    //     once the listener drops the sender (no more ticket forwards).
    let listener_result = listener_handle.await;
    let _ = drain_handle.await;
    // The signal task is best-effort (it returns after firing or on
    // handler-install failure); abort the join in case it's still running
    // because we already saw the shutdown fire.
    signal_handle.abort();
    let _ = signal_handle.await;

    // 16. Drain ticket tasks within the configured window.
    let window = Duration::from_secs(u64::from(cfg.engine.shutdown_window_seconds));
    let deadline = Instant::now() + window;

    let tickets_arc = dispatcher.tickets();
    // Drain the registry into a local Vec so we can release the lock for
    // each individual ticket-task await.
    let entries: Vec<(String, crate::daemon::dispatcher::TicketHandle)> = {
        let mut map = tickets_arc.lock().await;
        map.drain().collect()
    };

    let mut drained: usize = 0;
    let mut aborted_ticket_ids: Vec<String> = Vec::new();

    for (ticket_id, handle) in entries {
        let crate::daemon::dispatcher::TicketHandle { inbox, join } = handle;

        // Best-effort shutdown signal to the per-ticket task. If the
        // inbox is full or closed, dropping the sender below still wakes
        // the loop.
        let _ = inbox.send(DispatchMsg::Shutdown).await;
        drop(inbox);

        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, join).await {
            Ok(Ok(())) => {
                drained += 1;
            }
            Ok(Err(join_err)) => {
                // Task panicked; treat as aborted so the operator sees
                // it on the shutdown line.
                tracing::error!(
                    ticket_id = %ticket_id,
                    error = %join_err,
                    "ticket task join error during shutdown"
                );
                aborted_ticket_ids.push(ticket_id);
            }
            Err(_) => {
                aborted_ticket_ids.push(ticket_id);
            }
        }
    }

    let aborted = aborted_ticket_ids.len();

    {
        let mut w = daemon_events.lock().await;
        let _ = w.emit(&Event::DaemonShutdownCompleted {
            ts: now_rfc3339(),
            drained,
            aborted,
        });
    }

    // Surface listener errors as a tracing line; they do not change the
    // shutdown-window verdict.
    match listener_result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::error!(error = %err, "webhook listener errored during shutdown");
        }
        Err(join_err) => {
            tracing::error!(
                error = %join_err,
                "webhook listener task panicked during shutdown"
            );
        }
    }

    if aborted > 0 {
        let mut w = daemon_events.lock().await;
        let _ = w.emit(&Event::ShutdownWindowExceeded {
            ts: now_rfc3339(),
            aborted,
            aborted_ticket_ids,
        });
        return Err(SkeletonError::ShutdownWindowExceeded);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Startup-bound failure tests. The cycle-bound and happy-path coverage
    //! lives in the end-to-end smoke test (slice 1-4 e2e), with slice 5
    //! daemon-shutdown coverage added in Tasks 9-15.

    use super::*;
    use crate::error::{RokiConfigError, SkeletonError};
    use tempfile::TempDir;

    #[tokio::test]
    async fn missing_config_returns_config_error() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("does-not-exist.toml");

        match run_inner(&nonexistent, DispatchMode::Default).await {
            Err(SkeletonError::Config(RokiConfigError::MissingFile { path })) => {
                assert_eq!(path, nonexistent);
            }
            other => panic!("expected SkeletonError::Config(MissingFile), got {other:?}"),
        }
    }
}
