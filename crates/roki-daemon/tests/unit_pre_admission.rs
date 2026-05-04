//! Integration smoke tests for the pre-admission silent-skip judge.
//!
//! In-file unit tests in `tracker::pre_admission::tests` exhaust the 16-row
//! truth table; this file exercises the same `evaluate` surface plus the
//! mid-flight assignment-loss helper through the public API only.

use std::collections::BTreeSet;

use roki_daemon::orchestrator::state::Mode;
use roki_daemon::tracker::model::{
    IssueId, LABEL_ROKI_IMPL, LABEL_ROKI_READY, LinearLabel, LinearStateName, LinearUserId,
    NormalizedIssue,
};
use roki_daemon::tracker::pre_admission::{
    AdmissionDecision, PreAdmissionJudge, SkipReason, assignment_lost,
};

fn judge() -> PreAdmissionJudge {
    PreAdmissionJudge::new(
        LinearUserId::from("viewer-1"),
        BTreeSet::from([LinearStateName::from("Todo")]),
    )
}

fn issue(assignee: Option<&str>, state: &str, labels: &[&str]) -> NormalizedIssue {
    NormalizedIssue {
        issue: IssueId::from("ENG-1"),
        title: "t".to_owned(),
        body: "b".to_owned(),
        current_linear_state: LinearStateName::from(state),
        labels: labels.iter().map(|s| LinearLabel::from(*s)).collect(),
        assignee: assignee.map(LinearUserId::from),
    }
}

#[test]
fn admit_spec_driven_when_both_labels_present() {
    let admitted = judge().evaluate(&issue(
        Some("viewer-1"),
        "Todo",
        &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
    ));
    match admitted {
        AdmissionDecision::Admit { mode, .. } => assert_eq!(mode, Mode::SpecDriven),
        other => panic!("expected Admit(SpecDriven), got {other:?}"),
    }
}

#[test]
fn admit_needs_classify_when_only_ready_label_present() {
    let admitted = judge().evaluate(&issue(Some("viewer-1"), "Todo", &[LABEL_ROKI_READY]));
    match admitted {
        AdmissionDecision::Admit { mode, .. } => assert_eq!(mode, Mode::NeedsClassify),
        other => panic!("expected Admit(NeedsClassify), got {other:?}"),
    }
}

#[test]
fn skip_reasons_cover_each_documented_failure_path() {
    let j = judge();
    // 1. Assignee mismatch dominates.
    assert!(matches!(
        j.evaluate(&issue(Some("other"), "Todo", &[LABEL_ROKI_READY])),
        AdmissionDecision::Skip { reason: SkipReason::AssigneeMismatch }
    ));
    // 2. State outside admit set.
    assert!(matches!(
        j.evaluate(&issue(Some("viewer-1"), "Done", &[LABEL_ROKI_READY])),
        AdmissionDecision::Skip { reason: SkipReason::StateNotAdmitted }
    ));
    // 3. Missing roki:ready entirely.
    assert!(matches!(
        j.evaluate(&issue(Some("viewer-1"), "Todo", &[])),
        AdmissionDecision::Skip { reason: SkipReason::MissingRokiReady }
    ));
    // 4. roki:impl set without roki:ready surfaces a distinct skip reason.
    assert!(matches!(
        j.evaluate(&issue(Some("viewer-1"), "Todo", &[LABEL_ROKI_IMPL])),
        AdmissionDecision::Skip { reason: SkipReason::RokiImplWithoutRokiReady }
    ));
}

#[test]
fn assignment_lost_helper_detects_change_between_snapshots() {
    let prev = issue(Some("viewer-1"), "Todo", &[LABEL_ROKI_READY]);
    let still_assigned = prev.clone();
    let unassigned = issue(None, "Todo", &[LABEL_ROKI_READY]);
    let reassigned = issue(Some("viewer-2"), "Todo", &[LABEL_ROKI_READY]);

    assert!(!assignment_lost(&prev, &still_assigned));
    assert!(assignment_lost(&prev, &unassigned));
    assert!(assignment_lost(&prev, &reassigned));
}
