//! Integration smoke tests for the tracker surface.
//!
//! Drives the webhook router and the Linear poller against fakes (axum
//! tower service + wiremock) and asserts the DedupIndex absorbs duplicate
//! observations of the same issue id into a single LaunchFresh dispatch.
//!
//! The webhook in-file unit tests already cover signature verification at
//! the HTTP boundary; here we drive the bridge surface that downstream
//! orchestrator wiring depends on.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request as HttpRequest;
use hmac::{Hmac, Mac};
use roki_daemon::config::SecretValue;
use roki_daemon::orchestrator::state::Mode;
use roki_daemon::orchestrator::tracker_bridge::{DedupIndex, ObserveOutcome};
use roki_daemon::tracker::linear::{LinearClient, LinearPoller};
use roki_daemon::tracker::model::{
    IssueId, LABEL_ROKI_IMPL, LABEL_ROKI_READY, LinearLabel, LinearStateName, LinearUserId,
    NormalizedIssue,
};
use roki_daemon::tracker::pre_admission::{AdmissionDecision, PreAdmissionJudge};
use roki_daemon::tracker::webhook::{WebhookState, router};
use sha2::Sha256;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

type HmacSha256 = Hmac<Sha256>;

fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn issue_payload() -> serde_json::Value {
    serde_json::json!({
        "type": "Issue",
        "action": "update",
        "data": {
            "identifier": "ENG-77",
            "title": "Concurrent webhook + poll",
            "description": "body",
            "state": { "name": "Todo" },
            "labels": { "nodes": [
                { "name": LABEL_ROKI_READY },
                { "name": LABEL_ROKI_IMPL },
            ] },
            "assignee": { "id": "viewer-1" }
        }
    })
}

fn judge() -> PreAdmissionJudge {
    PreAdmissionJudge::new(
        LinearUserId::from("viewer-1"),
        std::collections::BTreeSet::from([LinearStateName::from("Todo")]),
    )
}

#[tokio::test]
async fn signed_webhook_request_dispatches_normalized_issue() {
    let secret = b"integration-secret".to_vec();
    let (tx, mut rx) = mpsc::channel(8);
    let state = Arc::new(WebhookState::new(
        SecretValue::new(String::from_utf8(secret.clone()).unwrap()),
        tx,
    ));

    let body = serde_json::to_vec(&issue_payload()).unwrap();
    let signature = sign(&secret, &body);
    let response = router(state)
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/linear/webhook")
                .header("Linear-Signature", signature)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let issue = rx.recv().await.expect("dispatched issue");
    assert_eq!(issue.issue, IssueId::from("ENG-77"));
    assert!(issue.has_roki_impl());
}

#[tokio::test]
async fn linear_poller_emits_normalized_issues_via_wiremock() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issues": { "nodes": [{
                "identifier": "ENG-77",
                "title": "title",
                "description": "body",
                "state": { "name": "Todo" },
                "labels": { "nodes": [
                    { "name": LABEL_ROKI_READY },
                    { "name": LABEL_ROKI_IMPL },
                ] },
                "assignee": { "id": "viewer-1" }
            }]}}
        })))
        .mount(&server)
        .await;

    let client = LinearClient::new(server.uri(), SecretValue::new("tok"))
        .with_backoff_floor(Duration::from_millis(5));
    let (sink_tx, mut sink_rx) = mpsc::channel(8);
    let (_refresh_tx, refresh_rx) = watch::channel(0u64);
    let poller = LinearPoller::new(
        client,
        LinearUserId::from("viewer-1"),
        vec![LinearStateName::from("Todo")],
        Duration::from_millis(50),
        sink_tx,
        refresh_rx,
    );

    let count = poller.poll_once().await.expect("poll_once");
    assert_eq!(count, 1);
    let issue = sink_rx.recv().await.expect("polled issue");
    assert_eq!(issue.issue, IssueId::from("ENG-77"));
    assert!(issue.has_roki_impl());
}

/// Webhook + poller both deliver the same `ENG-77` snapshot to the dedup
/// index. The first observation triggers `LaunchFresh`; the second is
/// absorbed as `UpdateInPlace`. This is the contract the orchestrator core
/// wiring depends on so a duplicate webhook+poll storm does not double-launch.
#[tokio::test]
async fn dedup_index_absorbs_duplicate_observations_of_same_issue_into_single_launch() {
    let index = DedupIndex::new();
    let issue = NormalizedIssue {
        issue: IssueId::from("ENG-77"),
        title: "Concurrent webhook + poll".to_owned(),
        body: "body".to_owned(),
        current_linear_state: LinearStateName::from("Todo"),
        labels: [LABEL_ROKI_READY, LABEL_ROKI_IMPL]
            .into_iter()
            .map(LinearLabel::from)
            .collect(),
        assignee: Some(LinearUserId::from("viewer-1")),
    };

    let judge = judge();
    let decision_a = judge.evaluate(&issue);
    assert!(matches!(decision_a, AdmissionDecision::Admit { mode: Mode::SpecDriven, .. }));
    let decision_b = judge.evaluate(&issue);

    let outcome_a = index.observe(issue.clone(), decision_a).await;
    let outcome_b = index.observe(issue.clone(), decision_b).await;

    match outcome_a {
        ObserveOutcome::LaunchFresh { mode, .. } => assert_eq!(mode, Mode::SpecDriven),
        other => panic!("expected LaunchFresh on first observation, got {other:?}"),
    }
    // Bind a synthetic orchestrator handle so `is_in_flight` flips on; the
    // second observation must take the in-flight refresh path.
    index.bind_orchestrator(&IssueId::from("ENG-77"), 99).await;
    let mut entry = index
        .snapshot(&IssueId::from("ENG-77"))
        .await
        .expect("dedup entry");
    // Promote to Active so observe() takes the in-flight path.
    entry.state = roki_daemon::orchestrator::state::WorkerState::Active;
    index.seed(issue.clone(), entry).await;

    // Re-observe the same snapshot; must be UpdateInPlace, not a second
    // LaunchFresh.
    let outcome_c = index.observe(issue.clone(), judge.evaluate(&issue)).await;
    assert!(
        matches!(outcome_c, ObserveOutcome::UpdateInPlace),
        "duplicate observation while in-flight must update in place, got {outcome_c:?}",
    );

    // Sanity-check first-shot dedup: the original duplicate observation
    // before binding also did not over-launch.
    assert!(
        matches!(outcome_b, ObserveOutcome::UpdateInPlace),
        "second observation while Pending must update in place, got {outcome_b:?}",
    );
}
