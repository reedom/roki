//! Task 10.1.5 integration test: runtime composition funnels webhook + poll
//! observations through `PreAdmissionJudge` and `DedupIndex` into the
//! orchestrator inbox, dropping pre-admission failures and routing mid-flight
//! stop signals (`AssignmentLost` / `RokiReadyRemoved`) to the dedicated
//! actor messages consumed by the dedup index.
//!
//! The seam used here is `runtime::testing::drive_admission_for_test`, which
//! mirrors the production `admission_pipe` task body — the same
//! `route_observation` routing logic runs in tests as in production. Tests
//! inject `NormalizedIssue` values directly so the suite does not stand up
//! the webhook server or the poller; both feed the same workspace-level
//! channel that the production pipe consumes.
//!
//! Spec refs: requirements.md Req 3.1, 3.2, 3.7, 3.8, 3.9, 3.10, 3.11, 3.12,
//! 3.13, 3.14; design.md "Daemon bootstrap" step 11.

use std::collections::BTreeSet;
use std::sync::Arc;

use roki_daemon::orchestrator::core::ActorMessage;
use roki_daemon::orchestrator::state::{IssueId, Mode, WorkerState};
use roki_daemon::orchestrator::tracker_bridge::{
    DedupEntry, DedupIndex, ObserveOutcome, TerminationReason,
};
use roki_daemon::runtime::testing::{
    RecordingInbox, drive_admission_for_test,
};
use roki_daemon::tracker::model::{
    LABEL_ROKI_IMPL, LABEL_ROKI_READY, LinearLabel, LinearStateName, LinearUserId,
    NormalizedIssue,
};
use roki_daemon::tracker::pre_admission::PreAdmissionJudge;
use tracing_test::traced_test;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn admit_states_default() -> BTreeSet<LinearStateName> {
    BTreeSet::from([LinearStateName::from("Todo")])
}

fn judge_for_test() -> PreAdmissionJudge {
    PreAdmissionJudge::new(LinearUserId::from("viewer-1"), admit_states_default())
}

fn issue(
    id: &str,
    assignee: Option<&str>,
    state: &str,
    labels: &[&str],
) -> NormalizedIssue {
    NormalizedIssue {
        issue: IssueId::from(id),
        title: id.to_owned(),
        body: "body".to_owned(),
        current_linear_state: LinearStateName::from(state),
        labels: labels.iter().map(|s| LinearLabel::from(*s)).collect(),
        assignee: assignee.map(LinearUserId::from),
    }
}

// ---------------------------------------------------------------------------
// 1. Admit-passing observation routes exactly one TrackerAdmit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_for_admit_passing_issue_routes_exactly_one_tracker_admit() {
    let judge = judge_for_test();
    let dedup = Arc::new(DedupIndex::new());
    let inbox = RecordingInbox::new();

    let admitted = issue("ENG-101", Some("viewer-1"), "Todo", &[LABEL_ROKI_READY]);
    let outcome = drive_admission_for_test(
        admitted.clone(),
        &judge,
        dedup.as_ref(),
        inbox.as_ref(),
    )
    .await;

    match outcome {
        ObserveOutcome::LaunchFresh { ref issue, mode } => {
            assert_eq!(issue.issue, IssueId::from("ENG-101"));
            assert_eq!(mode, Mode::NeedsClassify);
        }
        other => panic!("expected LaunchFresh, got {other:?}"),
    }

    let routed = inbox.snapshot().await;
    assert_eq!(routed.len(), 1, "exactly one inbox delivery");
    let (issue_id, message) = &routed[0];
    assert_eq!(issue_id, &IssueId::from("ENG-101"));
    match message {
        ActorMessage::TrackerAdmit { mode, repo } => {
            assert_eq!(*mode, Mode::NeedsClassify);
            assert!(repo.is_none(), "admission pipe must seed repo as None");
        }
        other => panic!("expected TrackerAdmit, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 2. Pre-admission-failing observation: no inbox delivery, skipped log fires
// ---------------------------------------------------------------------------

#[tokio::test]
#[traced_test]
async fn webhook_for_pre_admission_failure_drops_without_inbox_delivery() {
    let judge = judge_for_test();
    let dedup = Arc::new(DedupIndex::new());
    let inbox = RecordingInbox::new();

    // Wrong assignee triggers the AssigneeMismatch skip path.
    let bad_assignee = issue("ENG-202", Some("not-the-viewer"), "Todo", &[LABEL_ROKI_READY]);
    let outcome = drive_admission_for_test(
        bad_assignee,
        &judge,
        dedup.as_ref(),
        inbox.as_ref(),
    )
    .await;
    assert_eq!(outcome, ObserveOutcome::Drop);

    let routed = inbox.snapshot().await;
    assert!(
        routed.is_empty(),
        "pre-admission failure must NOT route an inbox message; routed: {routed:?}"
    );

    // The judge already emitted the canonical info-severity skip event.
    assert!(
        logs_contain("tracker.pre_admission.skipped"),
        "expected tracker.pre_admission.skipped log event"
    );
    assert!(
        logs_contain("assignee_mismatch"),
        "skipped log must carry the failed condition"
    );
}

// ---------------------------------------------------------------------------
// 3. Concurrent webhook + poll observations: at most one launch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_webhook_and_poll_observations_yield_single_launch() {
    let judge = Arc::new(judge_for_test());
    let dedup = Arc::new(DedupIndex::new());
    let inbox = RecordingInbox::new();

    let admitted = issue("ENG-303", Some("viewer-1"), "Todo", &[LABEL_ROKI_READY]);

    // Fire two observations of the same issue concurrently — one represents
    // the webhook receiver, the other the poller. Only the first should
    // produce LaunchFresh; the second must observe the in-flight entry and
    // return UpdateInPlace per the DedupIndex invariant.
    let judge_a = judge.clone();
    let dedup_a = dedup.clone();
    let inbox_a = inbox.clone();
    let issue_a = admitted.clone();
    let task_a = tokio::spawn(async move {
        drive_admission_for_test(issue_a, judge_a.as_ref(), dedup_a.as_ref(), inbox_a.as_ref())
            .await
    });
    let judge_b = judge.clone();
    let dedup_b = dedup.clone();
    let inbox_b = inbox.clone();
    let issue_b = admitted.clone();
    let task_b = tokio::spawn(async move {
        drive_admission_for_test(issue_b, judge_b.as_ref(), dedup_b.as_ref(), inbox_b.as_ref())
            .await
    });
    let outcomes = (task_a.await.unwrap(), task_b.await.unwrap());

    let mut launches = 0usize;
    let mut updates = 0usize;
    for outcome in [&outcomes.0, &outcomes.1] {
        match outcome {
            ObserveOutcome::LaunchFresh { .. } => launches += 1,
            ObserveOutcome::UpdateInPlace => updates += 1,
            other => panic!("unexpected concurrent outcome: {other:?}"),
        }
    }
    assert_eq!(launches, 1, "exactly one observation launches a fresh actor");
    assert_eq!(updates, 1, "the second observation refreshes in place");

    let admit_count = inbox.admit_count_for(&IssueId::from("ENG-303")).await;
    assert_eq!(
        admit_count, 1,
        "concurrent webhook + poll must route exactly one TrackerAdmit"
    );
}

// ---------------------------------------------------------------------------
// 4. Mid-flight assignment loss routes TrackerAssignmentLost
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mid_flight_assignment_loss_routes_tracker_assignment_lost() {
    let judge = judge_for_test();
    let dedup = Arc::new(DedupIndex::new());
    let inbox = RecordingInbox::new();

    let prior = issue("ENG-404", Some("viewer-1"), "Todo", &[LABEL_ROKI_READY]);
    // Seed an in-flight entry so the dedup index treats the next observation
    // as a mid-flight signal rather than a fresh admission.
    dedup
        .seed(
            prior.clone(),
            DedupEntry {
                state: WorkerState::Active,
                mode: Some(Mode::NeedsClassify),
                latest_normalized: prior.clone(),
                in_flight_orch: Some(1),
                in_flight_phase: None,
            },
        )
        .await;

    let lost = issue("ENG-404", None, "Todo", &[LABEL_ROKI_READY]);
    let outcome = drive_admission_for_test(
        lost,
        &judge,
        dedup.as_ref(),
        inbox.as_ref(),
    )
    .await;
    assert_eq!(
        outcome,
        ObserveOutcome::TerminateInFlight {
            reason: TerminationReason::AssignmentLost,
        }
    );

    let routed = inbox.snapshot().await;
    assert_eq!(routed.len(), 1);
    let (issue_id, message) = &routed[0];
    assert_eq!(issue_id, &IssueId::from("ENG-404"));
    assert!(
        matches!(message, ActorMessage::TrackerAssignmentLost),
        "expected TrackerAssignmentLost, got {message:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Mid-flight roki:ready removal routes TrackerRokiReadyRemoved
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mid_flight_roki_ready_removal_routes_tracker_roki_ready_removed() {
    let judge = judge_for_test();
    let dedup = Arc::new(DedupIndex::new());
    let inbox = RecordingInbox::new();

    let prior = issue(
        "ENG-505",
        Some("viewer-1"),
        "Todo",
        &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
    );
    dedup
        .seed(
            prior.clone(),
            DedupEntry {
                state: WorkerState::Pending,
                mode: Some(Mode::SpecDriven),
                latest_normalized: prior.clone(),
                in_flight_orch: Some(2),
                in_flight_phase: None,
            },
        )
        .await;

    // roki:ready dropped — the underlying skip reason no longer matters; the
    // dedup index returns TerminateInFlight regardless.
    let unlabeled = issue("ENG-505", Some("viewer-1"), "Todo", &[]);
    let outcome = drive_admission_for_test(
        unlabeled,
        &judge,
        dedup.as_ref(),
        inbox.as_ref(),
    )
    .await;
    assert_eq!(
        outcome,
        ObserveOutcome::TerminateInFlight {
            reason: TerminationReason::RokiReadyRemoved,
        }
    );

    let routed = inbox.snapshot().await;
    assert_eq!(routed.len(), 1);
    let (issue_id, message) = &routed[0];
    assert_eq!(issue_id, &IssueId::from("ENG-505"));
    assert!(
        matches!(message, ActorMessage::TrackerRokiReadyRemoved),
        "expected TrackerRokiReadyRemoved, got {message:?}"
    );
}
