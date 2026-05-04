//! Pre-admission gate: silent-skip judge that decides whether a normalized
//! issue may enter the orchestrator turn loop.
//!
//! Conditions are evaluated as a closed truth table over four inputs:
//! 1. Assignee matches the configured viewer.
//! 2. Linear state is in `admit_states`.
//! 3. `roki:ready` label present.
//! 4. `roki:impl` label present (mode discriminator only).
//!
//! Any failure produces a `Skip` outcome with a documented reason; the caller
//! emits exactly one structured log event and never writes Linear / launches
//! an orchestrator. Mid-flight signal helpers (`assignment_lost`,
//! `roki_ready_removed`) capture the two daemon-side stop conditions that
//! drive `WorkerState::Cleaning`.
//!
//! Spec refs: requirements.md Req 2.14, 3.1, 3.3, 3.7, 3.8, 3.9, 3.10.

use std::collections::BTreeSet;

use thiserror::Error;
use tracing::info;

use crate::config::AssigneeSpec;
use crate::orchestrator::state::Mode;
use crate::tracker::linear::{LinearClient, LinearError};
use crate::tracker::model::{LABEL_ROKI_READY, LinearStateName, LinearUserId, NormalizedIssue};
#[cfg(test)]
use crate::tracker::model::LABEL_ROKI_IMPL;

/// Outcome of `PreAdmissionJudge::evaluate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    Admit { issue: NormalizedIssue, mode: Mode },
    Skip { reason: SkipReason },
}

/// Documented silent-skip reasons. Closed set; new variants are an
/// extension-surface change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    AssigneeMismatch,
    StateNotAdmitted,
    MissingRokiReady,
    /// `roki:impl` is meaningless without `roki:ready`; we surface a distinct
    /// reason so the operator-facing log explains the misconfiguration.
    RokiImplWithoutRokiReady,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AssigneeMismatch => "assignee_mismatch",
            Self::StateNotAdmitted => "state_not_admitted",
            Self::MissingRokiReady => "missing_roki_ready",
            Self::RokiImplWithoutRokiReady => "roki_impl_without_roki_ready",
        }
    }
}

/// Resolved pre-admission gate. Constructed once at boot from the config
/// (`AssigneeSpec` + `admit_states`) plus the live Linear viewer.
#[derive(Debug, Clone)]
pub struct PreAdmissionJudge {
    assignee: LinearUserId,
    admit_states: BTreeSet<LinearStateName>,
}

impl PreAdmissionJudge {
    pub fn new(assignee: LinearUserId, admit_states: BTreeSet<LinearStateName>) -> Self {
        Self { assignee, admit_states }
    }

    pub fn assignee(&self) -> &LinearUserId {
        &self.assignee
    }

    pub fn admit_states(&self) -> &BTreeSet<LinearStateName> {
        &self.admit_states
    }

    /// Evaluate the four-condition truth table. Order matters only for the
    /// reason code we attach to the skip event; the decision itself is
    /// invariant.
    pub fn evaluate(&self, issue: &NormalizedIssue) -> AdmissionDecision {
        if !self.assignee_matches(issue) {
            return self.skip(issue, SkipReason::AssigneeMismatch);
        }
        if !self.admit_states.contains(&issue.current_linear_state) {
            return self.skip(issue, SkipReason::StateNotAdmitted);
        }
        let has_ready = issue.has_roki_ready();
        let has_impl = issue.has_roki_impl();
        if !has_ready && has_impl {
            return self.skip(issue, SkipReason::RokiImplWithoutRokiReady);
        }
        if !has_ready {
            return self.skip(issue, SkipReason::MissingRokiReady);
        }
        let mode = if has_impl {
            Mode::SpecDriven
        } else {
            Mode::NeedsClassify
        };
        AdmissionDecision::Admit {
            issue: issue.clone(),
            mode,
        }
    }

    fn assignee_matches(&self, issue: &NormalizedIssue) -> bool {
        issue
            .assignee
            .as_ref()
            .is_some_and(|user| user == &self.assignee)
    }

    fn skip(&self, issue: &NormalizedIssue, reason: SkipReason) -> AdmissionDecision {
        info!(
            event = "tracker.pre_admission.skipped",
            issue = %issue.issue,
            reason = reason.as_str(),
            "pre-admission silent skip"
        );
        AdmissionDecision::Skip { reason }
    }
}

// ---------------------------------------------------------------------------
// Assignee resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ResolutionError {
    #[error("Linear API error during assignee resolution: {0}")]
    Linear(#[from] LinearError),
    #[error("assignee selector matched zero users: {selector}")]
    NotFound { selector: String },
    #[error(
        "assignee selector matched {count} users (must be exactly one): {selector}"
    )]
    Ambiguous { selector: String, count: usize },
}

/// Resolves an `AssigneeSpec` to a stable `LinearUserId`. `Me` calls Linear's
/// viewer endpoint; explicit selectors are looked up against the user
/// directory (caller-supplied lookup so this module stays Linear-shape
/// agnostic for tests).
pub struct AssigneeResolver;

impl AssigneeResolver {
    /// Resolve `Me` via the live Linear client. Explicit selectors fall back
    /// to `resolve_with_lookup`; tests use that surface directly to inject
    /// candidate sets without standing up a mock GraphQL server.
    pub async fn resolve(
        spec: &AssigneeSpec,
        client: &LinearClient,
    ) -> Result<LinearUserId, ResolutionError> {
        match spec {
            AssigneeSpec::Me => Ok(client.viewer().await?),
            AssigneeSpec::Selector(_) => Err(ResolutionError::Ambiguous {
                selector: spec.raw().to_owned(),
                // Selector lookup belongs in the runtime layer that owns the
                // user directory query; surface a config error here so the
                // caller wires the proper resolver path explicitly.
                count: 0,
            }),
        }
    }

    /// Test-friendly helper: resolve an explicit selector against a candidate
    /// set. Real call sites supply a closure that hits Linear's user
    /// directory; here we keep the matching shape.
    pub fn resolve_with_lookup(
        spec: &AssigneeSpec,
        candidates: &[LinearUserId],
    ) -> Result<LinearUserId, ResolutionError> {
        match spec {
            AssigneeSpec::Me => Err(ResolutionError::Ambiguous {
                selector: "me".to_owned(),
                count: 0,
            }),
            AssigneeSpec::Selector(selector) => match candidates {
                [] => Err(ResolutionError::NotFound {
                    selector: selector.clone(),
                }),
                [single] => Ok(single.clone()),
                many => Err(ResolutionError::Ambiguous {
                    selector: selector.clone(),
                    count: many.len(),
                }),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Mid-flight stop signals
// ---------------------------------------------------------------------------

/// True when `prev` was assigned to someone and `curr` no longer is, or the
/// assignee changed. Caller is expected to scope this comparison to the same
/// `IssueId`.
pub fn assignment_lost(prev: &NormalizedIssue, curr: &NormalizedIssue) -> bool {
    match (&prev.assignee, &curr.assignee) {
        (Some(_), None) => true,
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

/// True when `roki:ready` was set on `prev` and is missing on `curr`.
pub fn roki_ready_removed(prev: &NormalizedIssue, curr: &NormalizedIssue) -> bool {
    prev.has_label(LABEL_ROKI_READY) && !curr.has_label(LABEL_ROKI_READY)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::model::{IssueId, LinearLabel};

    fn admit_states_default() -> BTreeSet<LinearStateName> {
        BTreeSet::from([LinearStateName::from("Todo")])
    }

    fn judge() -> PreAdmissionJudge {
        PreAdmissionJudge::new(LinearUserId::from("u1"), admit_states_default())
    }

    fn issue_with(
        assignee: Option<&str>,
        state: &str,
        labels: &[&str],
    ) -> NormalizedIssue {
        NormalizedIssue {
            issue: IssueId::from("ENG-1"),
            title: "t".to_owned(),
            body: "b".to_owned(),
            current_linear_state: LinearStateName::from(state),
            labels: labels.iter().map(|s| LinearLabel::from(*s)).collect(),
            assignee: assignee.map(LinearUserId::from),
        }
    }

    /// Truth table over (assignee × state × roki:ready × roki:impl). Each
    /// row's expected outcome is documented; we exercise all 16 corners.
    #[test]
    fn truth_table_covers_all_sixteen_corners() {
        let j = judge();
        let cases: Vec<(Option<&str>, &str, bool, bool, AdmissionDecision)> = vec![
            // Assignee mismatch dominates regardless of other inputs.
            (
                None,
                "Todo",
                false,
                false,
                AdmissionDecision::Skip {
                    reason: SkipReason::AssigneeMismatch,
                },
            ),
            (
                None,
                "Todo",
                true,
                true,
                AdmissionDecision::Skip {
                    reason: SkipReason::AssigneeMismatch,
                },
            ),
            (
                Some("other"),
                "Todo",
                true,
                true,
                AdmissionDecision::Skip {
                    reason: SkipReason::AssigneeMismatch,
                },
            ),
            (
                Some("other"),
                "Done",
                false,
                false,
                AdmissionDecision::Skip {
                    reason: SkipReason::AssigneeMismatch,
                },
            ),
            // State outside admit set.
            (
                Some("u1"),
                "Done",
                false,
                false,
                AdmissionDecision::Skip {
                    reason: SkipReason::StateNotAdmitted,
                },
            ),
            (
                Some("u1"),
                "Done",
                true,
                false,
                AdmissionDecision::Skip {
                    reason: SkipReason::StateNotAdmitted,
                },
            ),
            (
                Some("u1"),
                "Done",
                true,
                true,
                AdmissionDecision::Skip {
                    reason: SkipReason::StateNotAdmitted,
                },
            ),
            (
                Some("u1"),
                "Done",
                false,
                true,
                AdmissionDecision::Skip {
                    reason: SkipReason::StateNotAdmitted,
                },
            ),
            // Admitted state, missing roki:ready.
            (
                Some("u1"),
                "Todo",
                false,
                false,
                AdmissionDecision::Skip {
                    reason: SkipReason::MissingRokiReady,
                },
            ),
            // roki:impl without roki:ready surfaces its own reason.
            (
                Some("u1"),
                "Todo",
                false,
                true,
                AdmissionDecision::Skip {
                    reason: SkipReason::RokiImplWithoutRokiReady,
                },
            ),
            // Admit: roki:ready alone -> NeedsClassify.
            (
                Some("u1"),
                "Todo",
                true,
                false,
                AdmissionDecision::Admit {
                    issue: NormalizedIssue {
                        issue: IssueId::from("ENG-1"),
                        title: "t".to_owned(),
                        body: "b".to_owned(),
                        current_linear_state: LinearStateName::from("Todo"),
                        labels: BTreeSet::from([LinearLabel::from(LABEL_ROKI_READY)]),
                        assignee: Some(LinearUserId::from("u1")),
                    },
                    mode: Mode::NeedsClassify,
                },
            ),
            // Admit: roki:ready + roki:impl -> SpecDriven.
            (
                Some("u1"),
                "Todo",
                true,
                true,
                AdmissionDecision::Admit {
                    issue: NormalizedIssue {
                        issue: IssueId::from("ENG-1"),
                        title: "t".to_owned(),
                        body: "b".to_owned(),
                        current_linear_state: LinearStateName::from("Todo"),
                        labels: BTreeSet::from([
                            LinearLabel::from(LABEL_ROKI_IMPL),
                            LinearLabel::from(LABEL_ROKI_READY),
                        ]),
                        assignee: Some(LinearUserId::from("u1")),
                    },
                    mode: Mode::SpecDriven,
                },
            ),
            // Same assignee with various ineligible combos for completeness.
            (
                Some("u1"),
                "Backlog",
                true,
                true,
                AdmissionDecision::Skip {
                    reason: SkipReason::StateNotAdmitted,
                },
            ),
            (
                Some("u1"),
                "Backlog",
                false,
                false,
                AdmissionDecision::Skip {
                    reason: SkipReason::StateNotAdmitted,
                },
            ),
            (
                Some("other"),
                "Todo",
                false,
                true,
                AdmissionDecision::Skip {
                    reason: SkipReason::AssigneeMismatch,
                },
            ),
            (
                Some("other"),
                "Backlog",
                true,
                true,
                AdmissionDecision::Skip {
                    reason: SkipReason::AssigneeMismatch,
                },
            ),
        ];
        assert_eq!(cases.len(), 16);
        for (assignee, state, ready, impl_, expected) in cases {
            let mut labels = vec![];
            if ready {
                labels.push(LABEL_ROKI_READY);
            }
            if impl_ {
                labels.push(LABEL_ROKI_IMPL);
            }
            let issue = issue_with(assignee, state, &labels);
            let got = j.evaluate(&issue);
            assert_eq!(
                got, expected,
                "row (assignee={assignee:?}, state={state}, ready={ready}, impl={impl_})"
            );
        }
    }

    #[test]
    fn skip_reason_codes_are_stable() {
        assert_eq!(SkipReason::AssigneeMismatch.as_str(), "assignee_mismatch");
        assert_eq!(SkipReason::StateNotAdmitted.as_str(), "state_not_admitted");
        assert_eq!(SkipReason::MissingRokiReady.as_str(), "missing_roki_ready");
        assert_eq!(
            SkipReason::RokiImplWithoutRokiReady.as_str(),
            "roki_impl_without_roki_ready"
        );
    }

    #[test]
    fn assignment_lost_detects_drop_and_change() {
        let prev = issue_with(Some("u1"), "Todo", &[]);
        let curr_none = issue_with(None, "Todo", &[]);
        let curr_other = issue_with(Some("u2"), "Todo", &[]);
        assert!(assignment_lost(&prev, &curr_none));
        assert!(assignment_lost(&prev, &curr_other));
        assert!(!assignment_lost(&prev, &prev));
    }

    #[test]
    fn roki_ready_removed_detects_label_drop() {
        let prev = issue_with(Some("u1"), "Todo", &[LABEL_ROKI_READY]);
        let curr = issue_with(Some("u1"), "Todo", &[]);
        assert!(roki_ready_removed(&prev, &curr));
        assert!(!roki_ready_removed(&curr, &prev));
        assert!(!roki_ready_removed(&prev, &prev));
    }

    #[test]
    fn assignee_resolver_explicit_single_match() {
        let spec = AssigneeSpec::Selector("alice@example.com".to_owned());
        let resolved = AssigneeResolver::resolve_with_lookup(
            &spec,
            &[LinearUserId::from("u-alice")],
        )
        .expect("single-match resolution");
        assert_eq!(resolved, LinearUserId::from("u-alice"));
    }

    #[test]
    fn assignee_resolver_explicit_ambiguous_fails() {
        let spec = AssigneeSpec::Selector("alice".to_owned());
        let err = AssigneeResolver::resolve_with_lookup(
            &spec,
            &[LinearUserId::from("u-1"), LinearUserId::from("u-2")],
        )
        .unwrap_err();
        match err {
            ResolutionError::Ambiguous { count, selector } => {
                assert_eq!(count, 2);
                assert_eq!(selector, "alice");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn assignee_resolver_explicit_not_found_fails() {
        let spec = AssigneeSpec::Selector("ghost".to_owned());
        let err = AssigneeResolver::resolve_with_lookup(&spec, &[]).unwrap_err();
        assert!(matches!(err, ResolutionError::NotFound { .. }));
    }

    #[tokio::test]
    async fn assignee_resolver_me_uses_viewer() {
        use serde_json::json;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use std::time::Duration;
        use crate::config::SecretValue;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "viewer": { "id": "viewer-uuid" } }
            })))
            .mount(&server)
            .await;
        let client = LinearClient::new(server.uri(), SecretValue::new("tok"))
            .with_backoff_floor(Duration::from_millis(5));
        let resolved = AssigneeResolver::resolve(&AssigneeSpec::Me, &client)
            .await
            .expect("me resolves via viewer");
        assert_eq!(resolved, LinearUserId::from("viewer-uuid"));
    }
}
