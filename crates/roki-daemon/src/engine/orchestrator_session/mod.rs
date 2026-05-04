//! Orchestrator session adapter: long-lived `claude` session driving the
//! per-issue turn loop. This module hosts the action parser (schema types
//! consumed across crates) and the daemon -> orchestrator event payloads.
//!
//! Spec refs: requirements.md Req 4.x, 5.x; design.md "Components and
//! Interfaces" OrchestratorSessionAdapter section.

pub mod action_parser;
pub mod events;
