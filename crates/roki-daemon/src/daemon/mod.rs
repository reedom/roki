//! Persistent-daemon runtime layer (slice 5).
//!
//! Per-ticket diff cache, per-ticket actor task, dispatcher, and
//! shutdown coordinator. The cycle engine in `engine::*` is reused
//! unchanged.

pub mod cache;
pub mod shutdown;
// `dispatcher`, `ticket_task` are added in subsequent tasks.
