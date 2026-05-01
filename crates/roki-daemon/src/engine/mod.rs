//! Engine adapter for supervising long-lived `claude --print --output-format
//! stream-json` subprocesses.
//!
//! This module exports the stream-json parser (task 2.7) and the policy
//! controller (task 2.8). The subprocess supervisor and permission resolver
//! are delivered by sibling tasks (2.9, 2.10) and will live alongside
//! [`stream`] and [`policy`] in this module.

pub mod policy;
pub mod stream;

pub use stream::{EngineLifecycleEvent, parse_line};
