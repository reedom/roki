//! Tracker adapter root.
//!
//! The tracker is the daemon's read-only window onto Linear (Requirement 3.5
//! pins the read-only constraint). It produces a normalized stream of issue
//! events that the orchestrator consumes.
//!
//! Submodules:
//!
//! * [`model`] — the [`model::NormalizedIssue`] shape published by the
//!   adapter, plus the [`model::IssueState`] taxonomy that maps Linear
//!   workflow-state types to a small lifecycle bucket.
//! * [`linear`] — the polling Linear GraphQL adapter (task 2.5). The webhook
//!   hot-path lives in a sibling task (2.6) and shares the same `model`.
//!
//! Polling cadence cap (<= 5 min per scope, Requirement 3.2) and 429
//! exponential backoff (Requirement 3.3) are enforced inside [`linear`].

pub mod linear;
pub mod model;
