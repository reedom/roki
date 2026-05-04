//! Orchestrator core: per-issue state machine and transition contracts.
//!
//! The orchestrator owns the `WorkerState` lifecycle for every admitted
//! issue. This module hosts the canonical state, mode, transition trigger,
//! and `IssueId` newtype consumed by the tracker, engine, and observability
//! crates.
//!
//! Spec refs:
//! - design.md "Per-issue ticket lifecycle" (lines 332-362)
//! - requirements.md Req 2.6, 8.1, 8.2, 13.2

pub mod core;
pub mod escalation;
pub mod events;
pub mod hooks;
pub mod read;
pub mod state;
pub mod tracker_bridge;
