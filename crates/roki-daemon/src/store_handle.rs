//! Process-wide handle for the SQLite control-plane store.
//!
//! Mirrors the `observability::global_ring` pattern: `runtime::run_inner`
//! installs an `Arc<dyn Store>` once at startup so every `EventWriter::emit`
//! can dual-write without re-plumbing the handle through dispatcher,
//! per-ticket task, runner, cleanup, escalation, and orphan-reconcile code
//! paths. Tests that never install a store see `global_store()` return
//! `None` and pay only the OnceLock load.
//!
//! Phase-1 (event dual-write) only. Cycle FSM and admission cache move to
//! the store in later phases and will take an `Arc<dyn Store>` explicitly
//! instead of going through this singleton.

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use roki_store::Store;
use roki_store::models::NewEvent;

static STORE: OnceLock<Arc<dyn Store>> = OnceLock::new();

/// Install the process-wide store. Idempotent: first installer wins.
pub fn set_global_store(store: Arc<dyn Store>) -> bool {
    STORE.set(store).is_ok()
}

/// Borrow the process-wide store, if one has been installed.
pub fn global_store() -> Option<Arc<dyn Store>> {
    STORE.get().cloned()
}

/// Append an event to the global store if installed.
///
/// Errors are logged via `tracing::warn` and swallowed: the JSONL file
/// remains the authoritative event log in phase-1. Promoting the store
/// to the authoritative sink is gated on phase-2 / phase-3 work.
pub fn append_event_best_effort(
    store: &Arc<dyn Store>,
    writer_ticket_id: &str,
    ev: &crate::events::Event,
) {
    let (variant_ticket, _cycle_uuid) = ev.routing_keys();
    let ticket_id = variant_ticket.unwrap_or(writer_ticket_id).to_string();

    let payload = match serde_json::to_value(ev) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                event_kind = ev.kind_str(),
                error = %err,
                "store dual-write skipped: payload serialize failed"
            );
            return;
        }
    };

    let new_event = NewEvent {
        ticket_id,
        // Phase-1: cycle FSM still lives in the daemon process, so the
        // store has no row to point at. Phase-3 will resolve the uuid
        // against `cycles` and populate this field.
        cycle_id: None,
        ts: now_millis(),
        kind: ev.kind_str().to_string(),
        payload,
    };

    if let Err(err) = store.append_event(new_event) {
        tracing::warn!(
            event_kind = ev.kind_str(),
            error = %err,
            "store dual-write failed"
        );
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
