//! Integration tests for the Linear webhook receiver (task 2.6).
//!
//! Drives the axum router that the webhook module exposes through `tower`'s
//! `oneshot` so the test never starts a real listener. Each test shares a
//! single observation: the tracker sink either receives a normalized issue
//! or it does not, depending on the signature/body validity.
//!
//! Together these cases exercise Requirement 3.1 (signature verified before
//! normalization) and Requirement 3.4 (normalized shape unchanged from the
//! polling adapter).

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use hmac::{Hmac, Mac};
use roki_daemon::config::SecretString;
use roki_daemon::orchestrator::state::IssueId;
use roki_daemon::tracker::model::IssueState;
use roki_daemon::tracker::webhook::{
    DEFAULT_WEBHOOK_PATH, LINEAR_SIGNATURE_HEADER, WebhookState, router_default,
};
use serde_json::json;
use sha2::Sha256;
use tokio::sync::mpsc;
use tower::ServiceExt;

type HmacSha256 = Hmac<Sha256>;

const TEST_SECRET: &str = "test-webhook-secret";

fn signed_body_request(body: Vec<u8>, signature: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(DEFAULT_WEBHOOK_PATH)
        .header("content-type", "application/json")
        .header(LINEAR_SIGNATURE_HEADER, signature)
        .body(Body::from(body))
        .expect("request build")
}

fn unsigned_body_request(body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(DEFAULT_WEBHOOK_PATH)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request build")
}

fn hmac_hex(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac init");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn make_state(sink: mpsc::Sender<roki_daemon::tracker::model::NormalizedIssue>) -> WebhookState {
    WebhookState::new_workspace(SecretString::new(TEST_SECRET), sink)
}

fn issue_payload() -> serde_json::Value {
    json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": "uuid-here",
            "identifier": "ENG-123",
            "title": "Fix the thing",
            "description": "Body text",
            "state": { "type": "started", "name": "In Progress" },
            "assignee": { "id": "user-me" },
            "team": { "key": "ENG" },
            "labels": { "nodes": [ { "name": "bug" }, { "name": "p1" } ] }
        }
    })
}

#[tokio::test]
async fn correctly_signed_payload_emits_normalized_issue() {
    let (tx, mut rx) = mpsc::channel(8);
    let app = router_default(make_state(tx));

    let body_bytes = serde_json::to_vec(&issue_payload()).unwrap();
    let signature = hmac_hex(TEST_SECRET.as_bytes(), &body_bytes);

    let response = app
        .oneshot(signed_body_request(body_bytes, &signature))
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let received = rx.recv().await.expect("normalized issue dispatched");
    assert_eq!(received.issue, IssueId::new("ENG-123"));
    assert_eq!(received.title, "Fix the thing");
    assert_eq!(received.description, "Body text");
    assert_eq!(received.state, IssueState::Active);
    assert_eq!(received.labels, vec!["bug".to_string(), "p1".to_string()]);
    assert_eq!(received.assignee_user_id.as_deref(), Some("user-me"));
}

#[tokio::test]
async fn incorrectly_signed_payload_is_rejected_with_401() {
    let (tx, mut rx) = mpsc::channel(8);
    let app = router_default(make_state(tx));

    let body_bytes = serde_json::to_vec(&issue_payload()).unwrap();
    // Compute against a different secret so the signature is well-formed but
    // does not match.
    let bogus_signature = hmac_hex(b"not-the-secret", &body_bytes);

    let response = app
        .oneshot(signed_body_request(body_bytes, &bogus_signature))
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert!(
        body.is_empty(),
        "rejection responses must not echo payload content",
    );

    // Sink must not have observed anything.
    assert!(
        rx.try_recv().is_err(),
        "no normalization should occur on bad signature",
    );
}

#[tokio::test]
async fn missing_signature_header_is_rejected_with_401() {
    let (tx, mut rx) = mpsc::channel(8);
    let app = router_default(make_state(tx));

    let body_bytes = serde_json::to_vec(&issue_payload()).unwrap();

    let response = app
        .oneshot(unsigned_body_request(body_bytes))
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert!(body.is_empty());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn malformed_json_after_valid_signature_is_rejected_with_400() {
    let (tx, mut rx) = mpsc::channel(8);
    let app = router_default(make_state(tx));

    let body_bytes = b"this is not json".to_vec();
    // Sign the malformed body correctly so we exercise the post-signature
    // JSON-decode path rather than the signature path.
    let signature = hmac_hex(TEST_SECRET.as_bytes(), &body_bytes);

    let response = app
        .oneshot(signed_body_request(body_bytes, &signature))
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert!(body.is_empty());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn non_issue_event_types_are_acknowledged_without_dispatch() {
    let (tx, mut rx) = mpsc::channel(8);
    let app = router_default(make_state(tx));

    // Linear sends webhooks for many object types. The receiver should
    // acknowledge non-`Issue` events so Linear does not retry them, but
    // dispatch nothing to the tracker sink.
    let payload = json!({
        "action": "create",
        "type": "Comment",
        "data": { "id": "abc" }
    });
    let body_bytes = serde_json::to_vec(&payload).unwrap();
    let signature = hmac_hex(TEST_SECRET.as_bytes(), &body_bytes);

    let response = app
        .oneshot(signed_body_request(body_bytes, &signature))
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn hmac_mismatch_short_circuits_before_deserialization() {
    // Task 7.1c requirement (3): the single workspace HMAC secret rejects
    // mismatched signatures BEFORE the JSON body is parsed. We exercise this
    // by sending malformed JSON with a bad signature: if the handler tried
    // to deserialize first the response would be 400 (malformed JSON);
    // instead we must observe 401 (HMAC rejection short-circuits earlier).
    let (tx, mut rx) = mpsc::channel(8);
    let app = router_default(make_state(tx));

    let body_bytes = b"this is not json at all".to_vec();
    let bogus_signature = hmac_hex(b"not-the-real-secret", &body_bytes);

    let response = app
        .oneshot(signed_body_request(body_bytes, &bogus_signature))
        .await
        .expect("oneshot");

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "HMAC verification must fail closed before body is parsed",
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn single_workspace_route_accepts_correct_hmac() {
    // Task 7.1c requirement (1)+(2): there is exactly one webhook route at
    // `POST /linear/webhook` and it accepts payloads signed with the
    // workspace-level HMAC secret. The route MUST NOT include a per-repo
    // path segment.
    assert_eq!(DEFAULT_WEBHOOK_PATH, "/linear/webhook");

    let (tx, mut rx) = mpsc::channel(8);
    let app = router_default(make_state(tx));

    let body_bytes = serde_json::to_vec(&issue_payload()).unwrap();
    let signature = hmac_hex(TEST_SECRET.as_bytes(), &body_bytes);

    let response = app
        .oneshot(signed_body_request(body_bytes, &signature))
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let received = rx.recv().await.expect("normalized issue dispatched");
    assert_eq!(received.issue, IssueId::new("ENG-123"));
}

#[tokio::test]
async fn payload_content_is_not_echoed_in_error_responses() {
    // Even if the payload contains identifying tokens, they must never appear
    // in the response body for any rejection path.
    let (tx, _rx) = mpsc::channel(8);
    let app = router_default(make_state(tx));

    let payload = json!({
        "type": "Issue",
        "data": {
            "identifier": "ENG-SECRET-MARKER-9000",
            "title": "tripwire",
            "state": { "type": "started" }
        }
    });
    let body_bytes = serde_json::to_vec(&payload).unwrap();
    let bogus_signature = hmac_hex(b"wrong", &body_bytes);

    let response = app
        .oneshot(signed_body_request(body_bytes, &bogus_signature))
        .await
        .expect("oneshot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        !body_str.contains("ENG-SECRET-MARKER-9000"),
        "payload identifier must not be echoed; body was: {body_str:?}",
    );
    assert!(!body_str.contains("tripwire"));
}
