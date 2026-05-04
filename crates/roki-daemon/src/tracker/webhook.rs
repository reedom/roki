//! Linear webhook receiver: HMAC-SHA256 verification, JSON parsing, and
//! normalized-issue dispatch.
//!
//! Mounts a single endpoint, `POST /linear/webhook`. The body is read raw so
//! the signature compare runs over the bytes Linear actually signed; only
//! after verification do we parse JSON. Non-issue event types are
//! acknowledged with `200 OK` without dispatch — Linear retries on non-2xx,
//! and we don't want to hammer the daemon for events it doesn't model.
//!
//! Spec refs: requirements.md Req 3.1, 3.2.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use tokio::sync::mpsc;

use crate::config::SecretValue;
use crate::tracker::model::{
    IssueId, LinearLabel, LinearStateName, LinearUserId, NormalizedIssue,
};

type HmacSha256 = Hmac<Sha256>;

/// Maximum accepted webhook body. Linear payloads are well under 1 MiB; this
/// guard exists so a malformed sender cannot drive the daemon out of memory.
const MAX_BODY_BYTES: usize = 1_048_576;

/// Header name Linear uses for HMAC signatures. Matches Linear's documented
/// `Linear-Signature` header. Treat case-insensitively per HTTP semantics.
const SIGNATURE_HEADER: &str = "linear-signature";

/// Shared webhook state: the secret used to verify HMAC and the mpsc sink
/// that forwards normalized issues to the orchestrator bridge.
pub struct WebhookState {
    pub hmac_secret: SecretValue,
    pub sink: mpsc::Sender<NormalizedIssue>,
}

impl WebhookState {
    pub fn new(hmac_secret: SecretValue, sink: mpsc::Sender<NormalizedIssue>) -> Self {
        Self { hmac_secret, sink }
    }
}

/// Build the axum router for the webhook endpoint. The router is intentionally
/// minimal so the bootstrap layer can compose it with other routes (health,
/// metrics) without coupling them to the tracker module.
pub fn router(state: Arc<WebhookState>) -> Router {
    Router::new()
        .route("/linear/webhook", post(handle_webhook))
        .with_state(state)
}

async fn handle_webhook(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    request: Request,
) -> impl IntoResponse {
    let body = match to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };

    let signature = match extract_signature(&headers) {
        Some(s) => s,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if !verify_signature(state.hmac_secret.expose_secret().as_bytes(), &body, &signature) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        // Generic refusal — never echo body content so an attacker who guesses
        // a valid signature still cannot use the response as an oracle.
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    if !is_issue_event(&value) {
        // Linear emits Comment / Reaction / Project events through the same
        // hook; ack without dispatching.
        return StatusCode::OK.into_response();
    }

    let issue = match normalize_payload(&value) {
        Some(i) => i,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    if state.sink.send(issue).await.is_err() {
        // Receiver dropped — daemon is winding down. Surface 503 so Linear's
        // retry kicks the next live receiver after restart-recovery.
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    StatusCode::OK.into_response()
}

fn extract_signature(headers: &HeaderMap) -> Option<Vec<u8>> {
    let raw = headers
        .iter()
        .find(|(name, _)| name.as_str().eq_ignore_ascii_case(SIGNATURE_HEADER))
        .and_then(|(_, value)| value.to_str().ok())?;
    hex::decode(raw).ok()
}

fn verify_signature(secret: &[u8], body: &[u8], signature: &[u8]) -> bool {
    let mut mac = match HmacSha256::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    // `verify_slice` runs constant-time comparison against the expected MAC.
    mac.verify_slice(signature).is_ok()
}

fn is_issue_event(value: &Value) -> bool {
    // Linear's webhook envelope carries `type: "Issue"` for issue events.
    value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("Issue"))
}

fn normalize_payload(value: &Value) -> Option<NormalizedIssue> {
    // Linear ships the issue snapshot under `data`. We accept either
    // `identifier` (human form, e.g. `ENG-123`) or fall back to `id`.
    let data = value.get("data")?;
    let identifier = data
        .get("identifier")
        .and_then(Value::as_str)
        .or_else(|| data.get("id").and_then(Value::as_str))?;
    let title = data.get("title").and_then(Value::as_str).unwrap_or("");
    let body = data.get("description").and_then(Value::as_str).unwrap_or("");
    let state_name = data
        .pointer("/state/name")
        .and_then(Value::as_str)
        .or_else(|| data.get("stateName").and_then(Value::as_str))?;
    let labels: BTreeSet<LinearLabel> = data
        .pointer("/labels/nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| n.get("name").and_then(Value::as_str))
                .map(LinearLabel::from)
                .collect()
        })
        .or_else(|| {
            data.get("labels").and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|n| n.get("name").and_then(Value::as_str))
                    .map(LinearLabel::from)
                    .collect()
            })
        })
        .unwrap_or_default();
    let assignee = data
        .pointer("/assignee/id")
        .and_then(Value::as_str)
        .or_else(|| data.get("assigneeId").and_then(Value::as_str))
        .map(LinearUserId::from);

    Some(NormalizedIssue {
        issue: IssueId::from(identifier),
        title: title.to_owned(),
        body: body.to_owned(),
        current_linear_state: LinearStateName::from(state_name),
        labels,
        assignee,
    })
}

/// Test-only helper: compute the HMAC-SHA256 of a payload as a lowercase hex
/// string suitable for the `Linear-Signature` header.
#[cfg(test)]
pub(crate) fn sign_payload(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    hex::encode(bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use serde_json::json;
    use tower::ServiceExt;

    use crate::tracker::model::LABEL_ROKI_READY;

    fn make_state() -> (Arc<WebhookState>, mpsc::Receiver<NormalizedIssue>, Vec<u8>) {
        let secret_bytes = b"test-webhook-secret".to_vec();
        let (tx, rx) = mpsc::channel(8);
        let state = Arc::new(WebhookState::new(
            SecretValue::new(String::from_utf8(secret_bytes.clone()).unwrap()),
            tx,
        ));
        (state, rx, secret_bytes)
    }

    fn issue_payload() -> Value {
        json!({
            "type": "Issue",
            "action": "update",
            "data": {
                "identifier": "ENG-42",
                "title": "Add HMAC verification",
                "description": "## AC\n- verify",
                "state": { "name": "Todo" },
                "labels": { "nodes": [{ "name": LABEL_ROKI_READY }] },
                "assignee": { "id": "user-uuid-1" },
            }
        })
    }

    #[tokio::test]
    async fn valid_signed_payload_forwards_normalized_issue() {
        let (state, mut rx, secret) = make_state();
        let body = serde_json::to_vec(&issue_payload()).unwrap();
        let sig = sign_payload(&secret, &body);

        let response = router(state)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/linear/webhook")
                    .header("Linear-Signature", &sig)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let issue = rx.recv().await.expect("dispatched issue");
        assert_eq!(issue.issue, IssueId::from("ENG-42"));
        assert_eq!(issue.current_linear_state, LinearStateName::from("Todo"));
        assert!(issue.has_roki_ready());
        assert_eq!(
            issue.assignee.as_ref(),
            Some(&LinearUserId::from("user-uuid-1"))
        );
    }

    #[tokio::test]
    async fn tampered_body_returns_401() {
        let (state, mut rx, secret) = make_state();
        let body = serde_json::to_vec(&issue_payload()).unwrap();
        let sig = sign_payload(&secret, &body);
        let mut tampered = body.clone();
        tampered.extend_from_slice(b" "); // signature now invalid

        let response = router(state)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/linear/webhook")
                    .header("Linear-Signature", sig)
                    .body(Body::from(tampered))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn missing_signature_returns_401() {
        let (state, mut rx, _secret) = make_state();
        let body = serde_json::to_vec(&issue_payload()).unwrap();
        let response = router(state)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/linear/webhook")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn malformed_json_after_valid_signature_returns_400() {
        let (state, mut rx, secret) = make_state();
        let body = b"not json".to_vec();
        let sig = sign_payload(&secret, &body);
        let response = router(state)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/linear/webhook")
                    .header("Linear-Signature", sig)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_issue_event_returns_200_without_dispatch() {
        let (state, mut rx, secret) = make_state();
        let payload = json!({
            "type": "Comment",
            "data": { "id": "c-1", "body": "hi" }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let sig = sign_payload(&secret, &body);
        let response = router(state)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/linear/webhook")
                    .header("Linear-Signature", sig)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn error_responses_do_not_echo_payload() {
        let (state, _rx, _secret) = make_state();
        let secret_bytes = b"different-secret";
        let body = serde_json::to_vec(&issue_payload()).unwrap();
        // Sign with the wrong secret to force a 401.
        let bad_sig = sign_payload(secret_bytes, &body);
        let response = router(state)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/linear/webhook")
                    .header("Linear-Signature", bad_sig)
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response_bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&response_bytes);
        // Body must not echo the payload's identifier.
        assert!(
            !text.contains("ENG-42"),
            "401 response leaked payload content: {text:?}"
        );
    }

    #[tokio::test]
    async fn signature_header_is_case_insensitive() {
        let (state, mut rx, secret) = make_state();
        let body = serde_json::to_vec(&issue_payload()).unwrap();
        let sig = sign_payload(&secret, &body);
        let response = router(state)
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/linear/webhook")
                    .header("LINEAR-SIGNATURE", sig)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = rx.recv().await.expect("issue forwarded");
    }
}
