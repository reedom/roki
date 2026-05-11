//! Linear webhook receiver.
//!
//! Single-route HTTP receiver bound by `runtime` after configuration loads.
//!
//! The handler is path-agnostic — Linear's webhook URL is configured by the
//! operator, so any POST path is accepted. Body parse extracts the four
//! required fields (`data.id`, `data.assignee.id`, `data.state.name`,
//! `data.labels[].name`) from the Linear webhook envelope; missing fields
//! return HTTP 400 with a `tracing::warn!` line carrying an `error_id` for
//! log correlation. No HMAC verification is performed in the skeleton phase
//! even when `[linear.webhook].secret` is configured (Req 3.3).
//!
//! Cross-task state is carried by an `mpsc::Sender<NormalizedTicket>`
//! shared with the dispatcher (slice 5: capacity 64). On `try_send` Full
//! or Closed the response is HTTP 503 so Linear retries. Dedup of
//! unchanged tracked-triple webhooks happens further downstream in the
//! dispatcher's `DiffCache::observe` (`DiffOutcome::Unchanged` → skip).

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::WebhookError;
use crate::linear::ticket::NormalizedTicket;

/// Cold-start admission gate for the webhook listener.
///
/// While the gate is closed (default at construction), every inbound
/// request is short-circuited to HTTP 503 `cold_start_in_progress` so
/// Linear retries after the daemon finishes the cold-start enumerate +
/// dispatch + orphan reconcile pipeline. `runtime::run_inner` opens the
/// gate immediately before emitting `daemon_ready`.
#[derive(Clone)]
pub struct ReadyGate {
    open: Arc<AtomicBool>,
}

impl ReadyGate {
    pub fn new() -> Self {
        Self {
            open: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn open(&self) {
        self.open.store(true, Ordering::Release);
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}

impl Default for ReadyGate {
    fn default() -> Self {
        Self::new()
    }
}

async fn ready_gate_layer(State(gate): State<ReadyGate>, request: Request, next: Next) -> Response {
    if !gate.is_open() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "cold_start_in_progress"})),
        )
            .into_response();
    }
    next.run(request).await
}

/// Cross-task state shared between the axum handler and the dispatcher.
///
/// The matching `Receiver` is owned by `runtime::run_inner` and forwarded
/// to the dispatcher's `drain` task. The handler always forwards every
/// validly parsed ticket; backpressure surfaces as HTTP 503 when the
/// channel is full or the receiver has been dropped.
#[derive(Clone)]
pub struct WebhookState {
    pub sender: Arc<mpsc::Sender<NormalizedTicket>>,
    /// Polling-fallback outage signal: the webhook handler bumps this to
    /// `now_ms` on every successful 202, so `PollingTracker` can gate its
    /// outage ticks on `now_ms - last_ms >= 2 * cadence`. `None` keeps the
    /// existing test seams (router tests use no atom).
    pub last_webhook_success: Option<Arc<AtomicI64>>,
}

/// Build the axum `Router` for the webhook receiver.
///
/// Path-agnostic POST routing per design `linear::webhook`: the route `/`
/// and the wildcard `/*rest` both forward to `handle`. Other methods on
/// any path fall through to axum's default 405 response.
///
/// The router is wrapped in a `ready_gate_layer` middleware that
/// short-circuits to HTTP 503 `cold_start_in_progress` while
/// `gate.is_open()` is `false`. The runtime opens the gate immediately
/// before emitting `daemon_ready`.
pub fn router(state: WebhookState, gate: ReadyGate) -> Router {
    Router::new()
        .route("/", post(handle))
        .route("/*rest", post(handle))
        .with_state(state)
        .layer(middleware::from_fn_with_state(gate, ready_gate_layer))
}

async fn handle(State(state): State<WebhookState>, body: Bytes) -> Response {
    let ticket = match parse_ticket(&body) {
        Ok(ticket) => ticket,
        Err(reason) => return reject_invalid_payload(&reason),
    };

    match state.sender.try_send(ticket) {
        Ok(()) => {
            if let Some(atom) = &state.last_webhook_success {
                let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
                atom.store(now_ms, Ordering::Relaxed);
            }
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"status": "accepted"})),
            )
                .into_response()
        }
        // Channel buffer full: dispatcher hasn't drained yet. Linear
        // retries on 503 so the next POST gets through once the
        // dispatcher catches up.
        Err(mpsc::error::TrySendError::Full(_)) => service_unavailable("backpressure"),
        // Receiver dropped — daemon is shutting down.
        Err(mpsc::error::TrySendError::Closed(_)) => service_unavailable("shutting_down"),
    }
}

fn reject_invalid_payload(reason: &str) -> Response {
    let error_id = Uuid::new_v4().to_string();
    tracing::warn!(
        error_id = %error_id,
        reason = %reason,
        "webhook payload parse error",
    );
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "invalid_payload"})),
    )
        .into_response()
}

fn service_unavailable(reason: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": reason})),
    )
        .into_response()
}

/// Parse a Linear webhook body into a `NormalizedTicket`.
///
/// All four fields the skeleton consults must be present and string-typed.
/// Missing or wrong-typed fields surface as `Err(reason)`; the caller maps
/// that to HTTP 400 with an `error_id` log key.
fn parse_ticket(body: &[u8]) -> Result<NormalizedTicket, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|err| format!("invalid json: {err}"))?;

    let id = value
        .pointer("/data/id")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing data.id".to_string())?
        .to_string();

    let assignee_id = value
        .pointer("/data/assignee/id")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing data.assignee.id".to_string())?
        .to_string();

    let status = value
        .pointer("/data/state/name")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing data.state.name".to_string())?
        .to_string();

    let label_nodes = value
        .pointer("/data/labels")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing data.labels".to_string())?;
    let labels = label_nodes
        .iter()
        .map(|node| {
            node.pointer("/name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Option<Vec<String>>>()
        .ok_or_else(|| "missing data.labels[].name".to_string())?;

    // Title and body are not required by Linear's webhook schema for every
    // event kind; treat them as optional and default to empty so the
    // engine's Liquid context can still expand `{{ ticket.title }}` /
    // `{{ ticket.body }}` to an empty string for events that omit them.
    let title = value
        .pointer("/data/title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = value
        .pointer("/data/description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    Ok(NormalizedTicket::new(
        id,
        Some(assignee_id),
        status,
        labels,
        title,
        body,
    ))
}

/// Bind the axum listener and serve until `shutdown` resolves.
///
/// Bind failure surfaces as `WebhookError::BindFailed` carrying the
/// configured address (Req 3.1); a serve error during graceful shutdown
/// likewise surfaces as `BindFailed` (the typed surface conveys the addr
/// for the operator-visible log line).
pub async fn bind_and_serve(
    addr: SocketAddr,
    state: WebhookState,
    gate: ReadyGate,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), WebhookError> {
    let app = router(state, gate);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|source| WebhookError::BindFailed {
            addr: addr.to_string(),
            source,
        })?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|err| WebhookError::BindFailed {
            addr: addr.to_string(),
            source: std::io::Error::other(err),
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Router-level integration tests using `tower::ServiceExt::oneshot`,
    //! exercising every branch of the API and Event contracts: 400 on bad
    //! body, 202 on good body, 503 when the receiver is dropped, and
    //! one-202 / one-503 under concurrent good-body POSTs that exhaust a
    //! capacity-1 channel buffer.
    //!
    //! Tests cover Req 3.1, 3.2, 3.3, 3.4 at the router seam; the
    //! dispatcher's `DiffOutcome::Unchanged` short-circuit handles dedup
    //! at runtime.

    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn good_body() -> serde_json::Value {
        serde_json::json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "id": "tid-1",
                "assignee": {"id": "u1"},
                "state": {"name": "in_progress"},
                "labels": [{"name": "bug"}, {"name": "p0"}]
            }
        })
    }

    fn make_state() -> (WebhookState, mpsc::Receiver<NormalizedTicket>) {
        let (tx, rx) = mpsc::channel(1);
        let state = WebhookState {
            sender: Arc::new(tx),
            last_webhook_success: None,
        };
        (state, rx)
    }

    /// Test helper: a `ReadyGate` already in the open state, so the
    /// router-level tests exercise the post-cold-start happy paths
    /// (400 / 202 / 503-on-backpressure) rather than the
    /// `cold_start_in_progress` short-circuit.
    fn open_gate() -> ReadyGate {
        let g = ReadyGate::new();
        g.open();
        g
    }

    async fn post_json(app: Router, body: Vec<u8>) -> Response {
        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn post_to(app: Router, path: &str, body: Vec<u8>) -> Response {
        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn good_body_returns_202_and_emits_normalized_ticket() {
        let (state, mut rx) = make_state();
        let app = router(state, open_gate());

        let res = post_json(app, serde_json::to_vec(&good_body()).unwrap()).await;

        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let ticket = rx.recv().await.expect("receiver should observe ticket");
        assert_eq!(ticket.id, "tid-1");
        assert_eq!(ticket.assignee_id.as_deref(), Some("u1"));
        assert_eq!(ticket.status, "in_progress");
        assert_eq!(ticket.labels, vec!["bug".to_string(), "p0".to_string()]);
    }

    #[tokio::test]
    async fn good_body_on_arbitrary_path_returns_202() {
        // Path-agnostic routing per design `linear::webhook` (POST /*).
        let (state, mut rx) = make_state();
        let app = router(state, open_gate());

        let res = post_to(
            app,
            "/linear/webhook",
            serde_json::to_vec(&good_body()).unwrap(),
        )
        .await;

        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let ticket = rx.recv().await.expect("receiver should observe ticket");
        assert_eq!(ticket.id, "tid-1");
    }

    #[tokio::test]
    async fn malformed_json_returns_400_with_invalid_payload_body() {
        let (state, _rx) = make_state();
        let app = router(state, open_gate());

        let res = post_json(app, b"not json".to_vec()).await;

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(res.into_body(), 1024).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, serde_json::json!({"error": "invalid_payload"}));
    }

    #[tokio::test]
    async fn missing_id_returns_400() {
        let (state, _rx) = make_state();
        let app = router(state, open_gate());

        let mut body = good_body();
        body["data"].as_object_mut().unwrap().remove("id");
        let res = post_json(app, serde_json::to_vec(&body).unwrap()).await;

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_assignee_id_returns_400() {
        let (state, _rx) = make_state();
        let app = router(state, open_gate());

        let body = serde_json::json!({
            "data": {
                "id": "tid-1",
                "assignee": {},
                "state": {"name": "in_progress"},
                "labels": [{"name": "bug"}]
            }
        });
        let res = post_json(app, serde_json::to_vec(&body).unwrap()).await;

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_state_name_returns_400() {
        let (state, _rx) = make_state();
        let app = router(state, open_gate());

        let mut body = good_body();
        body["data"].as_object_mut().unwrap().remove("state");
        let res = post_json(app, serde_json::to_vec(&body).unwrap()).await;

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_label_node_name_returns_400() {
        let (state, _rx) = make_state();
        let app = router(state, open_gate());

        let body = serde_json::json!({
            "data": {
                "id": "tid-1",
                "assignee": {"id": "u1"},
                "state": {"name": "in_progress"},
                "labels": [{}]
            }
        });
        let res = post_json(app, serde_json::to_vec(&body).unwrap()).await;

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dropped_receiver_returns_503() {
        // Receiver dropped: TrySendError::Closed path. Surfaces the
        // "shutting_down" reason on the wire.
        let (state, rx) = make_state();
        drop(rx);
        let app = router(state, open_gate());

        let res = post_json(app, serde_json::to_vec(&good_body()).unwrap()).await;

        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn concurrent_good_posts_yield_one_202_and_one_503() {
        // Channel capacity 1 with no consumer draining: the first try_send
        // fills the buffer, the second observes TrySendError::Full → 503.
        // Holding `_rx` keeps the receiver alive so we exercise `Full`,
        // not `Closed`.
        let (state, _rx) = make_state();
        let app1 = router(state.clone(), open_gate());
        let app2 = router(state, open_gate());

        let body1 = serde_json::to_vec(&good_body()).unwrap();
        let body2 = serde_json::to_vec(&good_body()).unwrap();
        let (a, b) = tokio::join!(post_json(app1, body1), post_json(app2, body2));

        let mut codes = [a.status(), b.status()];
        codes.sort_by_key(|status| status.as_u16());
        assert_eq!(
            codes,
            [StatusCode::ACCEPTED, StatusCode::SERVICE_UNAVAILABLE]
        );
    }

    #[tokio::test]
    async fn parse_ticket_extracts_all_fields() {
        let bytes = serde_json::to_vec(&good_body()).unwrap();
        let ticket = parse_ticket(&bytes).expect("good body parses");
        assert_eq!(ticket.id, "tid-1");
        assert_eq!(ticket.assignee_id.as_deref(), Some("u1"));
        assert_eq!(ticket.status, "in_progress");
        assert_eq!(ticket.labels, vec!["bug".to_string(), "p0".to_string()]);
    }

    #[tokio::test]
    async fn good_body_propagates_title_and_description() {
        let (state, mut rx) = make_state();
        let app = router(state, open_gate());

        let mut body = good_body();
        body["data"]["title"] = serde_json::json!("Implement widget");
        body["data"]["description"] = serde_json::json!("Multi-line\ndescription");

        let res = post_json(app, serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let ticket = rx.recv().await.expect("ticket emitted");
        assert_eq!(ticket.title, "Implement widget");
        assert!(ticket.body.contains("description"));
    }

    #[tokio::test]
    async fn missing_title_and_description_default_to_empty() {
        let (state, mut rx) = make_state();
        let app = router(state, open_gate());

        // good_body() omits title/description; assert they default to "".
        let res = post_json(app, serde_json::to_vec(&good_body()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let ticket = rx.recv().await.expect("ticket emitted");
        assert_eq!(ticket.title, "");
        assert_eq!(ticket.body, "");
    }

    #[tokio::test]
    async fn closed_gate_short_circuits_to_503_cold_start_in_progress() {
        // Default-constructed `ReadyGate` is closed. While closed, the
        // middleware short-circuits every inbound request to HTTP 503
        // with `cold_start_in_progress`, regardless of body validity.
        let (state, _rx) = make_state();
        let app = router(state, ReadyGate::new());

        let res = post_json(app, serde_json::to_vec(&good_body()).unwrap()).await;

        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(res.into_body(), 1024).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({"error": "cold_start_in_progress"})
        );
    }

    #[tokio::test]
    async fn opening_gate_after_construction_admits_traffic() {
        let (state, mut rx) = make_state();
        let gate = ReadyGate::new();
        let app = router(state, gate.clone());
        gate.open();

        let res = post_json(app, serde_json::to_vec(&good_body()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let _ = rx.recv().await.expect("ticket emitted after gate opens");
    }
}
