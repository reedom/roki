#![allow(dead_code, unused_imports)]

//! Escalation queue (fr:06 §Escalation queue).

pub mod entry;
pub mod queue;
pub mod ring;

pub use entry::EscalationEntry;
pub use queue::EscalationQueue;
