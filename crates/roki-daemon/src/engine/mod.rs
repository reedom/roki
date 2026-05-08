//! Engine submodule: directive-driven cycle execution.
//!
//! Layered bottom-up:
//! - `outcome` — type vocabulary (PhaseKind, PhaseBody, directives, FailureKind).
//! - `directive` — last-JSON-object scan + per-phase legal-set validation.
//! - `template` — Liquid render for argv and stdin body.
//! - `context` — PhaseContext (Liquid object + ROKI_* env builder).
//! - `phase` — PhaseExecutor trait + the production CommandPhaseExecutor.
//! - `cycle` — run_cycle: iteration loop, transitions, iter cap.

pub mod cleanup;
pub mod context;
pub mod cycle;
pub mod directive;
pub mod dispatch;
pub mod on_failure;
pub mod outcome;
pub mod phase;
pub mod session;
pub mod stall;
pub mod stream;
pub mod template;
pub mod worktree;

pub use cycle::{CycleOutcome, run_cycle};
pub use phase::CommandPhaseExecutor;
#[allow(unused_imports)]
pub use session::{SessionConfig, SessionSupervisor};
