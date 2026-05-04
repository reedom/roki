//! Tracker domain model: `NormalizedIssue` and the small set of newtypes
//! that wrap stable Linear identifiers / labels / state names.
//!
//! `IssueId` lives canonically in `crate::orchestrator::state` and is
//! re-exported here so call sites can import it from the more natural
//! `tracker::model` namespace.
//!
//! Spec refs: requirements.md Req 3.5; design.md File Structure Plan
//! `tracker/model.rs`.

use std::collections::BTreeSet;
use std::fmt;

pub use crate::orchestrator::state::IssueId;

/// Recognized Linear label that admits an issue into the orchestrator turn
/// loop. Fixed name; not operator-configurable.
pub const LABEL_ROKI_READY: &str = "roki:ready";

/// Recognized Linear label routing the issue down the implementation path.
/// Fixed name; not operator-configurable.
pub const LABEL_ROKI_IMPL: &str = "roki:impl";

macro_rules! string_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

string_newtype!(LinearStateName, "Stable Linear workflow state name (e.g., `Todo`).");
string_newtype!(LinearLabel, "Stable Linear label name (e.g., `roki:ready`).");
string_newtype!(LinearUserId, "Stable Linear user identifier (UUID-shaped).");
string_newtype!(RepoId, "ghq-style repo identifier (`host/owner/repo`).");

/// Daemon-internal projection of a Linear issue. Construction lives in
/// `tracker::adapter` (task 3.x); here we just declare the shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedIssue {
    pub issue: IssueId,
    pub title: String,
    pub body: String,
    pub current_linear_state: LinearStateName,
    pub labels: BTreeSet<LinearLabel>,
    pub assignee: Option<LinearUserId>,
}

impl NormalizedIssue {
    /// Whether the issue carries the named label. Comparison is exact (Linear
    /// label names are case-sensitive).
    pub fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|label| label.0 == name)
    }

    /// Convenience for `has_label(LABEL_ROKI_READY)`.
    pub fn has_roki_ready(&self) -> bool {
        self.has_label(LABEL_ROKI_READY)
    }

    /// Convenience for `has_label(LABEL_ROKI_IMPL)`.
    pub fn has_roki_impl(&self) -> bool {
        self.has_label(LABEL_ROKI_IMPL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_issue(labels: &[&str]) -> NormalizedIssue {
        NormalizedIssue {
            issue: IssueId::from("ENG-101"),
            title: "Refactor the orchestrator turn loop".to_owned(),
            body: "## Acceptance Criteria\n1. ...".to_owned(),
            current_linear_state: LinearStateName::from("Todo"),
            labels: labels.iter().map(|name| LinearLabel::from(*name)).collect(),
            assignee: Some(LinearUserId::from("user-uuid-1")),
        }
    }

    #[test]
    fn has_roki_ready_and_has_roki_impl_round_trip() {
        let issue = sample_issue(&[LABEL_ROKI_READY, LABEL_ROKI_IMPL]);
        assert!(issue.has_roki_ready());
        assert!(issue.has_roki_impl());
    }

    #[test]
    fn has_label_is_case_sensitive() {
        let issue = sample_issue(&["Roki:Ready"]);
        assert!(!issue.has_roki_ready());
        assert!(issue.has_label("Roki:Ready"));
    }

    #[test]
    fn missing_labels_return_false() {
        let issue = sample_issue(&[]);
        assert!(!issue.has_roki_ready());
        assert!(!issue.has_roki_impl());
    }

    #[test]
    fn newtypes_round_trip_str_and_string() {
        let from_str: LinearLabel = "roki:ready".into();
        let from_string: LinearLabel = String::from("roki:ready").into();
        assert_eq!(from_str, from_string);
        assert_eq!(from_str.to_string(), "roki:ready");
    }
}
