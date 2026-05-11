//! In-memory observability primitives. Ring buffer + (future) hooks.

pub mod ring;

pub use ring::EventRing;

use std::sync::{Arc, OnceLock};

/// Process-wide singleton for the in-memory event ring.
///
/// `EventWriter::emit` consults this on the hot path so every file-backed
/// event is also routed into the ring without threading the `Arc<EventRing>`
/// through the dispatcher, per-ticket task, runner, cleanup, escalation,
/// and orphan-reconcile code paths. The ring is installed once by
/// `runtime::run_inner` right after construction; tests that need a clean
/// ring do not call `set_global_ring` and therefore see `global_ring()`
/// return `None` (the emit path becomes a no-op for the ring).
static EVENT_RING: OnceLock<Arc<EventRing>> = OnceLock::new();

/// Install the process-wide ring. Idempotent: subsequent calls are ignored
/// (the first writer wins). Returns `true` if this call installed the ring.
pub fn set_global_ring(ring: Arc<EventRing>) -> bool {
    EVENT_RING.set(ring).is_ok()
}

/// Borrow the process-wide ring, if one has been installed.
pub fn global_ring() -> Option<Arc<EventRing>> {
    EVENT_RING.get().cloned()
}
