//! Phase subprocess adapter: bounded `claude` subprocess per phase
//! nomination, driven by the catalog defaults and per-phase overrides.
//!
//! Spec refs: requirements.md Req 5.6, 5.12; design.md
//! `engine/phase_subprocess/`.

pub mod adapter;
pub mod catalog;
pub mod exit;
pub mod override_resolver;
pub mod policy;
