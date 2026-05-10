//! Engine submodule: state-machine cycle execution.
//!
//! Layered bottom-up:
//! - `outcome` — `FailureKind`, `CycleKind` vocabulary.
//! - `sentinel` — per-state directive-file control channel.
//! - `template` — Liquid render against a globals map.
//! - `state_runtime` — `StateRunner` trait + `CycleContext` + mock impl.
//! - `cycle_state` — drives a `StateMachine` to completion.
//! - `real_state_runner` — production `StateRunner` (subprocess spawn).
//! - `on_failure` — first-match routing of failure metadata.
//! - `cleanup` — worktree + session_tempdir teardown.

pub mod cleanup;
pub mod context;
pub mod cwd;
pub mod cycle_state;
pub mod dispatch;
pub mod on_failure;
pub mod outcome;
pub mod real_state_runner;
pub mod sentinel;
pub mod stall;
pub mod state_runtime;
pub mod stream;
pub mod template;
pub mod worktree;
