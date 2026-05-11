//! Persistent-daemon runtime layer (slice 5).
//!
//! Per-ticket diff cache, per-ticket actor task, dispatcher, and
//! shutdown coordinator. The cycle engine in `engine::*` is reused
//! unchanged.

pub mod cache;
pub mod cold_start;
pub mod deps;
pub mod cycle_metadata;
pub mod dispatcher;
pub mod orphan;
pub mod real_runner;
pub mod shutdown;
pub mod ticket_task;
