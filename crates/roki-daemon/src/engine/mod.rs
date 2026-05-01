//! Engine adapter for supervising long-lived `claude --print --output-format
//! stream-json` subprocesses.
//!
//! This module currently exports the stream-json parser (task 2.7). The
//! subprocess supervisor, policy controller, and permission resolver are
//! delivered by sibling tasks (2.8, 2.9, 2.10) and will live alongside
//! [`stream`] in this module.

pub mod stream;

pub use stream::{EngineLifecycleEvent, parse_line};
