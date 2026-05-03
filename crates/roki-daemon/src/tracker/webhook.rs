//! Linear webhook receiver (task 2.6).
//!
//! Implements the hot-path Linear receiver described in design.md
//! "TrackerAdapter — API Contract (webhook)":
//!
//! * Stands up an `axum` route at the configured path (default
//!   `/linear/webhook`).
//! * Verifies the `Linear-Signature` HMAC-SHA256 of the raw request body
//!   **before** any normalization (Requirement 3.1). This is enforced by
//!   reading the body as raw [`axum::body::Bytes`] and comparing in constant
//!   time before attempting JSON deserialization.
//! * Decodes the payload into the same [`NormalizedIssue`] shape the polling
//!   adapter publishes (Requirement 3.4) and dispatches it to the configured
//!   `mpsc::Sender<NormalizedIssue>` sink.
//! * Rejects unsigned, mismatched, or malformed payloads with the documented
//!   status codes (`401` and `400`) and an empty response body so payload
//!   contents are never echoed back.
//!
//! The receiver does not own its own HTTP server. Callers compose
//! [`router`] into their server (task 3.x will mount it under the daemon's
//! axum surface). The router handler holds a [`WebhookState`] consisting of
//! the shared HMAC secret, the routing context, and the tracker sink.
//!
//! Linear's webhook envelope is documented at
//! <https://developers.linear.app/docs/graphql/webhooks>. The receiver only
//! decodes the fields needed for [`NormalizedIssue`]; non-`Issue` event
//! types are acknowledged and ignored without dispatch.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::SecretString;
use crate::orchestrator::state::IssueId;
use crate::tracker::model::{IssueState, NormalizedIssue};

/// Default webhook path matching design.md's API Contract row.
pub const DEFAULT_WEBHOOK_PATH: &str = "/linear/webhook";

/// HTTP header Linear sets to the hex-encoded HMAC-SHA256 of the request body.
pub const LINEAR_SIGNATURE_HEADER: &str = "Linear-Signature";

type HmacSha256 = Hmac<Sha256>;

/// Shared state injected into the axum handler.
///
/// Post-task-7.1c the webhook receiver no longer pre-routes by repo — the
/// agent picks the repo on its first turn through `roki_open_worktree`
/// (see design-agent-driven-repo-selection.md). The single workspace-level
/// HMAC secret in `[linear].webhook_secret_env` keys the entire workspace.
///
/// Workspace-level webhook state (post-7.1f).
///
/// The legacy per-repo `repo` stamp and the `_team_or_scope_fallback`
/// constructor argument were both dropped: the agent picks the repo at
/// runtime via `roki_open_worktree`, so a daemon-side stamp on the
/// emitted `NormalizedIssue` conveyed nothing the orchestrator could rely
/// on. Construct via [`Self::new_workspace`].
#[derive(Clone)]
pub struct WebhookState {
    secret: SecretString,
    sink: mpsc::Sender<NormalizedIssue>,
}

impl WebhookState {
    /// Build a workspace-level [`WebhookState`].
    ///
    /// One HMAC secret, one webhook route, one sink — the 7.1f bootstrap
    /// composes exactly one of these for the entire daemon. Per the
    /// agent-driven design, no per-repo stamp is emitted on the
    /// [`NormalizedIssue`].
    pub fn new_workspace(secret: SecretString, sink: mpsc::Sender<NormalizedIssue>) -> Self {
        Self { secret, sink }
    }
}

/// Build the webhook [`Router`] for the daemon's HTTP surface.
///
/// Callers mount the returned router into their own server (the MVP daemon
/// only exposes this single endpoint; broader HTTP surfaces are out of scope
/// per the boundary in requirements.md).
pub fn router(state: WebhookState, path: &str) -> Router {
    Router::new()
        .route(path, post(handle_webhook))
        .with_state(state)
}

/// Convenience: a router mounted at [`DEFAULT_WEBHOOK_PATH`].
pub fn router_default(state: WebhookState) -> Router {
    router(state, DEFAULT_WEBHOOK_PATH)
}

/// Verify a hex-encoded HMAC-SHA256 signature against the raw body.
///
/// Returns `true` only when the signature is well-formed hex and matches the
/// HMAC of `body` keyed by `secret`. Comparison uses [`Mac::verify_slice`],
/// which is constant-time for a length-matched input. Length mismatches
/// return `false` immediately because `verify_slice` rejects them; this is
/// safe because the length of an HMAC-SHA256 tag is fixed (32 bytes / 64 hex
/// chars), so a length mismatch reveals nothing useful.
pub fn verify_signature(secret: &[u8], body: &[u8], signature_hex: &str) -> bool {
    let Ok(decoded) = hex::decode(signature_hex.trim()) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&decoded).is_ok()
}

async fn handle_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Verify signature BEFORE any deserialization (Requirement 3.1).
    let Some(signature_value) = headers.get(LINEAR_SIGNATURE_HEADER) else {
        warn!(target: "tracker::webhook", "rejected webhook with missing Linear-Signature header");
        return StatusCode::UNAUTHORIZED;
    };
    let Ok(signature_hex) = signature_value.to_str() else {
        warn!(target: "tracker::webhook", "rejected webhook with non-ASCII Linear-Signature header");
        return StatusCode::UNAUTHORIZED;
    };
    if !verify_signature(state.secret.expose().as_bytes(), &body, signature_hex) {
        warn!(target: "tracker::webhook", "rejected webhook with invalid signature");
        return StatusCode::UNAUTHORIZED;
    }

    // Signature has been validated; now we may parse the body. Two-stage
    // decode: first peek at the discriminator (`type`) so non-`Issue` event
    // types are acknowledged without insisting they conform to the issue
    // envelope schema. Linear sends webhooks for many object types, and the
    // MVP receiver only models the `Issue` shape (Requirement 3.4).
    let header: WebhookHeader = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            warn!(
                target: "tracker::webhook",
                error = %err,
                "rejected webhook with malformed JSON",
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    if !header.event_type.eq_ignore_ascii_case("Issue") {
        debug!(
            target: "tracker::webhook",
            event_type = %header.event_type,
            "ignoring non-Issue webhook event",
        );
        return StatusCode::NO_CONTENT;
    }

    let envelope: WebhookEnvelope = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            warn!(
                target: "tracker::webhook",
                error = %err,
                "rejected Issue webhook with malformed payload",
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    let normalized = normalize(envelope, &state);
    if let Err(err) = state.sink.send(normalized).await {
        // The orchestrator dropped the receiver. Log and acknowledge so
        // Linear does not retry forever; the daemon is shutting down.
        warn!(
            target: "tracker::webhook",
            error = %err,
            "tracker sink closed; dropping webhook event",
        );
    }
    StatusCode::NO_CONTENT
}

fn normalize(envelope: WebhookEnvelope, _state: &WebhookState) -> NormalizedIssue {
    let WebhookEnvelope { data, .. } = envelope;
    let labels = data
        .labels
        .map(|envelope| {
            envelope
                .nodes
                .into_iter()
                .map(|node| node.name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let bucket = IssueState::from_linear_type(data.state.kind.as_deref().unwrap_or(""));

    NormalizedIssue {
        issue: IssueId::new(data.identifier),
        title: data.title,
        description: data.description.unwrap_or_default(),
        state: bucket,
        labels,
        assignee_user_id: data.assignee.map(|assignee| assignee.id),
    }
}

#[derive(Debug, Deserialize)]
struct WebhookHeader {
    #[serde(rename = "type")]
    event_type: String,
}

#[derive(Debug, Deserialize)]
struct WebhookEnvelope {
    data: WebhookIssueData,
    // Linear sends `action`, `type`, and other metadata fields too; we only
    // need `data` after the header dispatch above.
}

#[derive(Debug, Deserialize)]
struct WebhookIssueData {
    identifier: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: WebhookStateField,
    #[serde(default)]
    labels: Option<WebhookLabelsEnvelope>,
    #[serde(default)]
    assignee: Option<WebhookAssigneeField>,
}

#[derive(Debug, Deserialize)]
struct WebhookAssigneeField {
    id: String,
}

#[derive(Debug, Deserialize)]
struct WebhookStateField {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WebhookLabelsEnvelope {
    #[serde(default)]
    nodes: Vec<WebhookLabelNode>,
}

#[derive(Debug, Deserialize)]
struct WebhookLabelNode {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hmac_hex(secret: &[u8], body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("hmac init");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn verify_signature_accepts_correct_signature() {
        let secret = b"shhh";
        let body = b"{\"hello\":\"world\"}";
        let sig = hmac_hex(secret, body);

        assert!(verify_signature(secret, body, &sig));
    }

    #[test]
    fn verify_signature_rejects_tampered_body() {
        let secret = b"shhh";
        let body = b"{\"hello\":\"world\"}";
        let tampered = b"{\"hello\":\"WORLD\"}";
        let sig = hmac_hex(secret, body);

        assert!(!verify_signature(secret, tampered, &sig));
    }

    #[test]
    fn verify_signature_rejects_wrong_secret() {
        let body = b"payload";
        let sig = hmac_hex(b"correct", body);

        assert!(!verify_signature(b"wrong", body, &sig));
    }

    #[test]
    fn verify_signature_rejects_non_hex_signature() {
        // Non-hex characters short-circuit before any HMAC computation.
        assert!(!verify_signature(b"secret", b"body", "ZZZZ-not-hex"));
    }

    #[test]
    fn verify_signature_rejects_length_mismatch() {
        // A correctly-hex but short signature must not match an HMAC-SHA256
        // (32 byte) tag. `verify_slice` rejects length mismatches.
        assert!(!verify_signature(b"secret", b"body", "deadbeef"));
    }

    #[test]
    fn verify_signature_tolerates_surrounding_whitespace() {
        let secret = b"shhh";
        let body = b"payload";
        let sig = hmac_hex(secret, body);
        let padded = format!("  {sig}\n");

        assert!(verify_signature(secret, body, &padded));
    }
}
