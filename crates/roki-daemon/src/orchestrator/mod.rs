//! Orchestrator core.
//!
//! The orchestrator owns the per-`(repo, issue)` state machine that drives a
//! Linear issue from discovery to a terminal state and the workspace cleanup
//! interim. This module re-exports the foundational state types so downstream
//! modules (event bus, worker actor, recovery reconciler, read projection,
//! pre-cleanup hook registry) can depend on a single canonical surface.
//!
//! Today this module exposes only `state` — the pure state, transition table,
//! and event-shape definitions added by task 2.1. Subsequent tasks (2.1a,
//! 3.1, 3.2, 3.5) layer additional submodules (`read`, `hooks`, `worker`,
//! `event_bus`) on top without renaming what is published here.

pub mod core;
pub mod events;
pub mod hooks;
pub mod read;
pub mod recovery;
pub mod state;
pub mod tracker_bridge;
