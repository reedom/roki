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
pub mod webhook;

use std::time::Duration;

use async_trait::async_trait;

use crate::tracker::linear::TrackerError;

/// Nudge-only refresh surface published by the tracker adapter
/// (Requirement 13.3, design.md "TrackerAdapter — Service Interface").
///
/// External callers — for example the `POST /api/v1/refresh` handler in
/// roki-observability or the webhook receiver in task 2.6 — can request that
/// the next per-scope poll be scheduled sooner. The trait exposes no read or
/// mutation surface beyond that request: it cannot bypass the documented
/// 5-minute per-scope cadence cap, and it cannot shorten an active 429
/// exponential-backoff window.
#[async_trait]
pub trait TrackerRefresh: Send + Sync {
    /// Request an out-of-cycle poll. Returns the window within which the
    /// next poll will occur. When the tracker is in 429 backoff, the
    /// returned window is the remaining backoff window — the nudge does not
    /// shorten the backoff. When the tracker is idle within its cadence, the
    /// returned window is approximately zero because the nudge advances the
    /// next-poll deadline to "now".
    async fn nudge(&self) -> Result<RefreshAccepted, TrackerError>;
}

/// Response shape for [`TrackerRefresh::nudge`].
///
/// `will_poll_within` names the window within which polling will occur after
/// the nudge is accepted. The field is the maximum remaining wait across all
/// scopes the tracker is watching, so a caller observing
/// `will_poll_within == 0` can be confident every scope will fire on its
/// next loop iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshAccepted {
    pub will_poll_within: Duration,
}
