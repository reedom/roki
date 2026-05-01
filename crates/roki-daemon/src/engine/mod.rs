//! Engine adapter for supervising long-lived `claude --print --output-format
//! stream-json` subprocesses.
//!
//! This module exports the stream-json parser (task 2.7), the policy
//! controller (task 2.8), and the subprocess supervisor (task 2.10). The
//! permission resolver lives next to it in [`crate::permissions`] (task 2.9).

pub mod claude;
pub mod policy;
pub mod stream;

pub use claude::{
    ClaudeEngineAdapter, LaunchError, PRELUDE_ADDITIONAL_CONTEXT_KEY, PRELUDE_CLOSE, PRELUDE_OPEN,
    PRELUDE_TOOLS_KEY, SupervisedEvent, WorkerContext, build_session_input,
};
pub use stream::{EngineLifecycleEvent, parse_line};
