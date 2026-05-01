//! Tracker domain model.
//!
//! The MVP collapses Linear's workflow-state taxonomy onto a four-bucket
//! [`IssueState`] enum (Requirement 3.4 only requires "current state", not the
//! entire Linear lexicon). Downstream routing and the orchestrator state
//! machine live in `orchestrator/state.rs`; this enum is the tracker-side view.
//!
//! [`NormalizedIssue`] is the structured issue contract emitted by every
//! tracker (polling today, webhook in task 2.6). Fields mirror design.md
//! "TrackerAdapter":
//!
//! * `repo` — the routed `RepoId` for this issue (the tracker today is
//!   parameterised per scope so it knows the repo at emit time; the
//!   orchestrator-side router will replace this when overlapping scopes land
//!   in task 1.5).
//! * `issue` — the Linear human-readable identifier (`ENG-123`).
//! * `title`, `description` — display fields.
//! * `state` — bucketed state (Requirement 3.4).
//! * `labels` — every label name attached to the issue (Requirement 3.4).
//! * `team_or_scope` — the Linear team key the issue belongs to
//!   (Requirement 3.4: "team or scope identifier").

use crate::orchestrator::state::{IssueId, RepoId};

/// Bucketed lifecycle state surfaced by the tracker.
///
/// The tracker MUST NOT leak Linear's full state taxonomy upstream — the
/// orchestrator only needs to know which lifecycle bucket an issue currently
/// occupies so it can drive the per-`(repo, issue)` state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IssueState {
    /// Linear `unstarted` / `started` types — the issue is in active
    /// workflow.
    Active,
    /// Linear `started` issues that the tracker recognises as the
    /// "awaiting review" bucket. Reserved for the webhook task; the polling
    /// adapter currently does not distinguish review from active.
    Review,
    /// Linear `completed` / `canceled` types — the issue is done.
    Terminal,
    /// Anything else (e.g. `triage`, `backlog`) the orchestrator does not
    /// route through a worker. Surfaced so the upstream consumer can log
    /// without losing data, not so the orchestrator can act on it.
    Other,
}

impl IssueState {
    /// Map a Linear workflow-state `type` string to the bucketed state.
    ///
    /// Linear documents the canonical types as `triage`, `backlog`,
    /// `unstarted`, `started`, `completed`, `canceled`. Anything outside that
    /// set falls into [`IssueState::Other`].
    pub fn from_linear_type(linear_type: &str) -> Self {
        match linear_type {
            "unstarted" | "started" => Self::Active,
            "completed" | "canceled" => Self::Terminal,
            _ => Self::Other,
        }
    }
}

/// Normalized issue event surfaced by the tracker.
///
/// The shape is the contract pinned by Requirement 3.4 and design.md. The
/// orchestrator subscribes to a stream of these and treats them as
/// idempotent on `(repo, issue, state)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedIssue {
    pub repo: RepoId,
    pub issue: IssueId,
    pub title: String,
    pub description: String,
    pub state: IssueState,
    pub labels: Vec<String>,
    pub team_or_scope: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_unstarted_and_started_to_active() {
        assert_eq!(
            IssueState::from_linear_type("unstarted"),
            IssueState::Active
        );
        assert_eq!(IssueState::from_linear_type("started"), IssueState::Active);
    }

    #[test]
    fn maps_completed_and_canceled_to_terminal() {
        assert_eq!(
            IssueState::from_linear_type("completed"),
            IssueState::Terminal,
        );
        assert_eq!(
            IssueState::from_linear_type("canceled"),
            IssueState::Terminal,
        );
    }

    #[test]
    fn maps_unknown_state_to_other() {
        assert_eq!(IssueState::from_linear_type("triage"), IssueState::Other);
        assert_eq!(IssueState::from_linear_type("backlog"), IssueState::Other);
        assert_eq!(IssueState::from_linear_type(""), IssueState::Other);
    }
}
