//! Engine submodule: directive-driven cycle execution.
//!
//! Layered bottom-up:
//! - `outcome` — type vocabulary (PhaseKind, PhaseBody, directives, FailureKind).
//! - `directive` — last-JSON-object scan + per-phase legal-set validation.
//! - `template` — Liquid render for argv and stdin body.
//! - `context` — PhaseContext (Liquid object + ROKI_* env builder).
//! - `phase` — PhaseExecutor trait + the production CommandPhaseExecutor.
//! - `cycle` — run_cycle: iteration loop, transitions, iter cap.

pub mod context;
pub mod cycle;
pub mod directive;
pub mod outcome;
pub mod phase;
pub mod stream;
pub mod template;

pub use cycle::{run_cycle, CycleOutcome};
pub use phase::CommandPhaseExecutor;
