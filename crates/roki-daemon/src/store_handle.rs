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

/// Run `f` with the process-wide store when installed. Swallows the
/// "no store" branch so callers do not have to litter every cycle-FSM site
/// with `if let Some(store) = global_store()`. Errors are best-effort:
/// they are surfaced via `tracing::warn` with the supplied `op` label and
/// never propagate into FSM control flow (phase-3 store writes are
/// observation-only).
pub fn with_store<F>(op: &'static str, f: F)
where
    F: FnOnce(&Arc<dyn Store>) -> roki_store::Result<()>,
{
    let Some(store) = global_store() else {
        return;
    };
    if let Err(err) = f(&store) {
        tracing::warn!(
            store_op = op,
            error = %err,
            "store write failed (best-effort)"
        );
    }
}

/// Unix milliseconds since the epoch, saturating to 0 on system clocks
/// before 1970. Reused by every store-touching site so the timestamp
/// vocabulary stays consistent across phase-2/3 writes.
pub fn now_unix_millis() -> i64 {
    now_millis()
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
    let (variant_ticket, cycle_uuid) = ev.routing_keys();
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
        // Phase-3: forward the variant's routing-key cycle UUID into the
        // store column so events join back to the `cycles` row the cycle
        // driver opened. Variants without a cycle context still write
        // `NULL` (e.g. daemon lifecycle events).
        cycle_id: cycle_uuid.map(|u| u.to_string()),
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
