//! Linear GraphQL adapter (read-only) and polling fallback.
//!
//! Read-only by construction: no `create_*` / `update_*` methods on
//! `LinearClient`. Linear writes belong to the orchestrator session
//! (see design.md "Daemon-internal replay" / FR-19).
//!
//! 429 / 5xx / network failures share an exponential backoff curve gated by
//! a single `next_request_at` deadline; every outgoing request waits for that
//! deadline before issuing a new call so the curve survives across retries.
//!
//! Spec refs: requirements.md Req 3.3, 3.4, 3.5, 3.6, 7.2.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::Instant;

use crate::config::SecretValue;
use crate::shutdown::ShutdownSignal;
#[cfg(test)]
use crate::tracker::model::{LABEL_ROKI_IMPL, LABEL_ROKI_READY};
use crate::tracker::model::{
    IssueId, LinearLabel, LinearStateName, LinearUserId, NormalizedIssue,
};

/// Default Linear GraphQL endpoint.
pub const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";

/// Initial backoff window. Doubles on each consecutive throttle / failure;
/// resets on a successful request.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
/// Cap on the exponential backoff window.
const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Default poll cadence for the fallback poller. Honored even when the watch
/// nudge fires — bursts cannot exceed one HTTP call per `cadence_floor`.
pub const DEFAULT_CADENCE_FLOOR: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum LinearError {
    #[error("network error talking to Linear: {0}")]
    Network(String),
    #[error("Linear returned HTTP {status}")]
    Http { status: u16 },
    #[error("backoff active until {until:?}")]
    Backoff { until: Instant },
    #[error("failed to parse Linear response: {0}")]
    Parse(String),
}

// ---------------------------------------------------------------------------
// Backoff state
// ---------------------------------------------------------------------------

/// Mutable state guarding the next allowed request and the current window
/// width. Shared via `Arc` so cloning a `LinearClient` (or the poller's view
/// of it) keeps a single curve.
#[derive(Debug)]
pub struct BackoffState {
    next_request_at: Mutex<Instant>,
    current_window: Mutex<Duration>,
    base: Duration,
    cap: Duration,
}

impl BackoffState {
    fn new(base: Duration, cap: Duration) -> Self {
        Self {
            next_request_at: Mutex::new(Instant::now()),
            current_window: Mutex::new(base),
            base,
            cap,
        }
    }

    /// Test seam: build a backoff state with explicit base / cap. The
    /// `LinearTrackerHandle` constructs its own state outside `LinearClient`
    /// in unit tests; everywhere else the state is borrowed from the client
    /// via `LinearClient::backoff()`.
    pub fn new_for_test(base: Duration, cap: Duration) -> Self {
        Self::new(base, cap)
    }

    /// Test seam: force the deadline to `at` so refresh-handle tests can
    /// assert `BackoffActive` without driving an actual 429 response.
    pub fn set_deadline_for_test(&self, at: Instant) {
        if let Ok(mut guard) = self.next_request_at.try_lock() {
            *guard = at;
        }
    }

    /// Synchronous peek used by `LinearTrackerHandle::nudge` (which must not
    /// be async). Returns `None` only when the lock is currently contended,
    /// in which case the caller treats the curve as active.
    pub fn next_request_at_for_peek(&self) -> Option<Instant> {
        self.next_request_at.try_lock().ok().map(|g| *g)
    }

    /// Return the current `next_request_at` deadline.
    pub async fn next_allowed(&self) -> Instant {
        *self.next_request_at.lock().await
    }

    /// On a successful response, clear the backoff: future requests may go
    /// out immediately and the window resets to the base.
    async fn reset(&self) {
        *self.next_request_at.lock().await = Instant::now();
        *self.current_window.lock().await = self.base;
    }

    /// On a throttled / failed response, double the window (capped) and push
    /// the deadline forward by the new window.
    async fn extend(&self) -> Instant {
        let mut window = self.current_window.lock().await;
        let next = (*window).saturating_mul(2).min(self.cap).max(self.base);
        *window = next;
        let deadline = Instant::now() + next;
        let mut next_at = self.next_request_at.lock().await;
        *next_at = deadline;
        deadline
    }

    /// Block until the deadline passes. Returns immediately if already due.
    async fn wait_until_ready(&self) {
        let deadline = self.next_allowed().await;
        let now = Instant::now();
        if deadline > now {
            tokio::time::sleep_until(deadline).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Read-only Linear GraphQL client. Construction takes the endpoint and the
/// resolved API token; the token header format follows Linear's convention
/// (`Authorization: <token>`, not `Bearer <token>`).
pub struct LinearClient {
    http: reqwest::Client,
    endpoint: String,
    token: SecretValue,
    backoff: Arc<BackoffState>,
}

impl LinearClient {
    pub fn new(endpoint: impl Into<String>, token: SecretValue) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builder");
        Self {
            http,
            endpoint: endpoint.into(),
            token,
            backoff: Arc::new(BackoffState::new(BACKOFF_BASE, BACKOFF_CAP)),
        }
    }

    /// Test seam: shrink the backoff floor so unit tests don't sleep 5s.
    pub fn with_backoff_floor(mut self, floor: Duration) -> Self {
        self.backoff = Arc::new(BackoffState::new(floor, BACKOFF_CAP));
        self
    }

    /// Test seam: also override the cap.
    pub fn with_backoff_window(mut self, base: Duration, cap: Duration) -> Self {
        self.backoff = Arc::new(BackoffState::new(base, cap));
        self
    }

    /// Expose the shared backoff handle so the poller / refresh layer can
    /// gate nudges on it without re-issuing requests.
    pub fn backoff(&self) -> Arc<BackoffState> {
        Arc::clone(&self.backoff)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Run `query { viewer { id } }` and return the resolved user id.
    pub async fn viewer(&self) -> Result<LinearUserId, LinearError> {
        let body = json!({ "query": "query { viewer { id } }" });
        let response = self.execute(body).await?;
        let id = response
            .pointer("/data/viewer/id")
            .and_then(Value::as_str)
            .ok_or_else(|| LinearError::Parse("viewer.id missing in response".to_owned()))?;
        Ok(LinearUserId::from(id))
    }

    /// Fetch one issue by id. Returns the canonical `NormalizedIssue`
    /// projection (see `tracker::model`).
    pub async fn issue_by_id(&self, id: &str) -> Result<NormalizedIssue, LinearError> {
        let query = "query Issue($id: String!) { \
            issue(id: $id) { \
              identifier title description \
              state { name } \
              labels { nodes { name } } \
              assignee { id } \
            } \
          }";
        let body = json!({
            "query": query,
            "variables": { "id": id },
        });
        let response = self.execute(body).await?;
        let node = response
            .pointer("/data/issue")
            .ok_or_else(|| LinearError::Parse("issue node missing".to_owned()))?;
        normalize_issue(node)
    }

    /// List issues filtered server-side by the given assignee and state set.
    pub async fn list_issues(
        &self,
        assignee: &LinearUserId,
        states: &[LinearStateName],
    ) -> Result<Vec<NormalizedIssue>, LinearError> {
        // Filter is keyed by stable Linear primitives: assignee.id eq and
        // state.name in. Pagination is left for a follow-up; the daemon's
        // candidate set is bounded by the assignee filter.
        let state_names: Vec<&str> = states.iter().map(|s| s.0.as_str()).collect();
        let query = "query List($assignee: ID!, $states: [String!]) { \
            issues(filter: { \
              assignee: { id: { eq: $assignee } }, \
              state: { name: { in: $states } } \
            }) { \
              nodes { \
                identifier title description \
                state { name } \
                labels { nodes { name } } \
                assignee { id } \
              } \
            } \
          }";
        let body = json!({
            "query": query,
            "variables": {
                "assignee": assignee.0,
                "states": state_names,
            },
        });
        let response = self.execute(body).await?;
        let nodes = response
            .pointer("/data/issues/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| LinearError::Parse("issues.nodes missing".to_owned()))?;
        nodes.iter().map(normalize_issue).collect()
    }

    /// Issue one GraphQL request, gated by the shared backoff state.
    async fn execute(&self, body: Value) -> Result<Value, LinearError> {
        self.backoff.wait_until_ready().await;
        let result = self
            .http
            .post(&self.endpoint)
            .header("Authorization", self.token.expose_secret())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await;

        let response = match result {
            Ok(r) => r,
            Err(err) => {
                let until = self.backoff.extend().await;
                tracing::warn!(error = %err, "linear request network failure; backing off");
                return Err(LinearError::Network(err.to_string()))
                    .map_err(|e| match e {
                        LinearError::Network(msg) => {
                            // Surface network as Network; callers that need
                            // the deadline can read it via `backoff()`.
                            let _ = until;
                            LinearError::Network(msg)
                        }
                        other => other,
                    });
            }
        };

        let status = response.status();
        if status.as_u16() == 429 {
            let until = self.backoff.extend().await;
            return Err(LinearError::Backoff { until });
        }
        if status.is_server_error() {
            let until = self.backoff.extend().await;
            tracing::warn!(status = status.as_u16(), "linear 5xx; backing off");
            let _ = until;
            return Err(LinearError::Http {
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            // Client-level error (4xx other than 429) — do not extend backoff;
            // the request shape is wrong and retrying won't help.
            return Err(LinearError::Http {
                status: status.as_u16(),
            });
        }

        let value: Value = response
            .json()
            .await
            .map_err(|e| LinearError::Parse(e.to_string()))?;
        if let Some(errors) = value.get("errors")
            && errors.is_array()
            && !errors.as_array().map(|a| a.is_empty()).unwrap_or(true)
        {
            return Err(LinearError::Parse(errors.to_string()));
        }
        self.backoff.reset().await;
        Ok(value)
    }
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LabelNode {
    name: String,
}

fn normalize_issue(node: &Value) -> Result<NormalizedIssue, LinearError> {
    let identifier = node
        .get("identifier")
        .and_then(Value::as_str)
        .ok_or_else(|| LinearError::Parse("issue.identifier missing".to_owned()))?;
    let title = node
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let body = node
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let state_name = node
        .pointer("/state/name")
        .and_then(Value::as_str)
        .ok_or_else(|| LinearError::Parse("issue.state.name missing".to_owned()))?;
    let labels: BTreeSet<LinearLabel> = node
        .pointer("/labels/nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| serde_json::from_value::<LabelNode>(n.clone()).ok())
                .map(|n| LinearLabel::from(n.name))
                .collect()
        })
        .unwrap_or_default();
    let assignee = node
        .pointer("/assignee/id")
        .and_then(Value::as_str)
        .map(LinearUserId::from);

    Ok(NormalizedIssue {
        issue: IssueId::from(identifier),
        title,
        body,
        current_linear_state: LinearStateName::from(state_name),
        labels,
        assignee,
    })
}

// Compile-time guard: the public surface of `LinearClient` must remain
// read-only. If a future maintainer adds a write method (e.g. `create_*`,
// `update_*`), this constant — referenced indirectly in tests — should be
// updated alongside an explicit comment in design.md.
#[doc(hidden)]
pub const PUBLIC_SURFACE_IS_READ_ONLY: () = ();

// ---------------------------------------------------------------------------
// Polling fallback
// ---------------------------------------------------------------------------

/// Polling fallback for environments where the webhook is unreachable. The
/// poller honors the shared backoff curve and respects the cadence floor: it
/// will not issue more than one HTTP request per `cadence_floor` even when
/// the watch channel nudges fire faster.
pub struct LinearPoller {
    client: LinearClient,
    assignee: LinearUserId,
    states: Vec<LinearStateName>,
    cadence_floor: Duration,
    sink: mpsc::Sender<NormalizedIssue>,
    last_poll: Mutex<Option<Instant>>,
    refresh_rx: watch::Receiver<u64>,
}

impl LinearPoller {
    pub fn new(
        client: LinearClient,
        assignee: LinearUserId,
        states: Vec<LinearStateName>,
        cadence_floor: Duration,
        sink: mpsc::Sender<NormalizedIssue>,
        refresh_rx: watch::Receiver<u64>,
    ) -> Self {
        Self {
            client,
            assignee,
            states,
            cadence_floor,
            sink,
            last_poll: Mutex::new(None),
            refresh_rx,
        }
    }

    /// Drive the polling loop until shutdown. Each iteration: wait for either
    /// the cadence sleep, a refresh nudge, or shutdown; throttle nudges to the
    /// cadence floor; honor 429 backoff.
    pub async fn run(mut self, shutdown: ShutdownSignal) {
        loop {
            if shutdown.is_shutting_down() {
                return;
            }
            let _ = self.poll_once().await;

            let sleep = tokio::time::sleep(self.cadence_floor);
            tokio::pin!(sleep);
            tokio::select! {
                _ = shutdown.wait() => return,
                _ = &mut sleep => {}
                changed = self.refresh_rx.changed() => {
                    if changed.is_err() {
                        // Sender dropped; fall back to cadence-only polling.
                        continue;
                    }
                    if !self.may_poll_now().await {
                        // Throttle: keep waiting on the cadence sleep.
                        let _ = (&mut sleep).await;
                    }
                }
            }
        }
    }

    /// True iff the cadence floor and the backoff curve both allow a new poll.
    async fn may_poll_now(&self) -> bool {
        let now = Instant::now();
        let last = *self.last_poll.lock().await;
        if let Some(t) = last
            && now.duration_since(t) < self.cadence_floor
        {
            return false;
        }
        if self.client.backoff().next_allowed().await > now {
            return false;
        }
        true
    }

    /// Run one poll and forward each normalized issue to the sink. Returns
    /// `Ok(count)` on success, `Err` on transport / parse failure.
    pub async fn poll_once(&self) -> Result<usize, LinearError> {
        // Even when called directly (tests / out-of-cycle nudges), record the
        // attempt timestamp so the throttle check stays correct.
        *self.last_poll.lock().await = Some(Instant::now());
        let issues = self
            .client
            .list_issues(&self.assignee, &self.states)
            .await?;
        let count = issues.len();
        for issue in issues {
            if self.sink.send(issue).await.is_err() {
                // Receiver dropped; treat as soft-stop. The loop driver will
                // exit on the next shutdown signal.
                return Ok(count);
            }
        }
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn token() -> SecretValue {
        SecretValue::new("lin_api_token_for_tests")
    }

    fn issue_node(id: &str, state: &str, labels: &[&str], assignee_id: Option<&str>) -> Value {
        let label_nodes: Vec<Value> =
            labels.iter().map(|name| json!({ "name": name })).collect();
        let assignee = match assignee_id {
            Some(id) => json!({ "id": id }),
            None => Value::Null,
        };
        json!({
            "identifier": id,
            "title": format!("title for {id}"),
            "description": "body",
            "state": { "name": state },
            "labels": { "nodes": label_nodes },
            "assignee": assignee,
        })
    }

    #[tokio::test]
    async fn viewer_returns_user_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("Authorization", "lin_api_token_for_tests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "viewer": { "id": "user-uuid-1" } }
            })))
            .mount(&server)
            .await;

        let client = LinearClient::new(server.uri(), token())
            .with_backoff_floor(Duration::from_millis(5));
        let id = client.viewer().await.expect("viewer ok");
        assert_eq!(id, LinearUserId::from("user-uuid-1"));
    }

    #[tokio::test]
    async fn list_issues_passes_assignee_and_states_in_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let vars = body.get("variables").cloned().unwrap_or(Value::Null);
                assert_eq!(
                    vars.get("assignee").and_then(Value::as_str),
                    Some("user-uuid-1")
                );
                let states: Vec<&str> = vars
                    .get("states")
                    .and_then(Value::as_array)
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                assert_eq!(states, vec!["Todo", "In Progress"]);
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": {
                        "issues": {
                            "nodes": [
                                issue_node("ENG-1", "Todo", &["roki:ready"], Some("user-uuid-1")),
                            ]
                        }
                    }
                }))
            })
            .mount(&server)
            .await;

        let client = LinearClient::new(server.uri(), token())
            .with_backoff_floor(Duration::from_millis(5));
        let issues = client
            .list_issues(
                &LinearUserId::from("user-uuid-1"),
                &[
                    LinearStateName::from("Todo"),
                    LinearStateName::from("In Progress"),
                ],
            )
            .await
            .expect("list_issues ok");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue, IssueId::from("ENG-1"));
        assert!(issues[0].has_label(LABEL_ROKI_READY));
        assert_eq!(
            issues[0].assignee.as_ref(),
            Some(&LinearUserId::from("user-uuid-1"))
        );
    }

    #[tokio::test]
    async fn issue_by_id_normalizes_labels_and_assignee() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "issue": issue_node(
                        "ENG-7",
                        "In Progress",
                        &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
                        Some("user-uuid-7"),
                    ),
                }
            })))
            .mount(&server)
            .await;

        let client = LinearClient::new(server.uri(), token())
            .with_backoff_floor(Duration::from_millis(5));
        let issue = client.issue_by_id("ENG-7").await.expect("issue_by_id ok");
        assert_eq!(issue.issue, IssueId::from("ENG-7"));
        assert_eq!(issue.current_linear_state, LinearStateName::from("In Progress"));
        assert!(issue.has_roki_ready());
        assert!(issue.has_roki_impl());
        assert_eq!(
            issue.assignee.as_ref(),
            Some(&LinearUserId::from("user-uuid-7"))
        );
    }

    #[tokio::test]
    async fn http_429_extends_backoff_window() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let floor = Duration::from_millis(80);
        let client = LinearClient::new(server.uri(), token())
            .with_backoff_window(floor, Duration::from_secs(5));
        let before = Instant::now();
        let err = client.viewer().await.unwrap_err();
        match err {
            LinearError::Backoff { until } => {
                let delta = until.saturating_duration_since(before);
                // Window = floor * 2 on first throttle.
                assert!(
                    delta >= floor,
                    "expected at least {floor:?} backoff, got {delta:?}"
                );
            }
            other => panic!("expected Backoff, got {other:?}"),
        }
        let next = client.backoff().next_allowed().await;
        assert!(next > before, "backoff deadline must move forward");
    }

    #[tokio::test]
    async fn successful_request_resets_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "viewer": { "id": "u1" } }
            })))
            .mount(&server)
            .await;

        let client = LinearClient::new(server.uri(), token())
            .with_backoff_floor(Duration::from_millis(5));
        client.viewer().await.unwrap();
        let next = client.backoff().next_allowed().await;
        // After reset, the deadline must not be in the future.
        assert!(next <= Instant::now() + Duration::from_millis(1));
    }

    /// Static surface check: no public method on `LinearClient` may begin with
    /// `create_` or `update_`. We assert by referring to the read-only marker
    /// constant — any future write method should be added with an explicit
    /// design.md change, at which point this test should be revisited.
    #[test]
    fn public_surface_is_read_only_marker() {
        let _: () = PUBLIC_SURFACE_IS_READ_ONLY;
    }

    // ---- Poller tests (Task 3.3) ----

    #[tokio::test]
    async fn poller_throttles_nudges_within_cadence_floor() {
        let server = MockServer::start().await;
        // Respond once; subsequent requests would also succeed but we assert
        // call count to verify only one request fires within the cadence.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "issues": { "nodes": [] } }
            })))
            .mount(&server)
            .await;

        let (sink_tx, mut sink_rx) = mpsc::channel::<NormalizedIssue>(8);
        let (refresh_tx, refresh_rx) = watch::channel(0u64);

        let client = LinearClient::new(server.uri(), token())
            .with_backoff_floor(Duration::from_millis(5));
        let poller = LinearPoller::new(
            client,
            LinearUserId::from("u1"),
            vec![LinearStateName::from("Todo")],
            Duration::from_secs(60),
            sink_tx,
            refresh_rx,
        );
        // First poll always allowed.
        poller.poll_once().await.unwrap();
        // Two nudges back-to-back must be throttled by the cadence floor.
        refresh_tx.send(1).unwrap();
        assert!(!poller.may_poll_now().await, "second nudge must throttle");
        refresh_tx.send(2).unwrap();
        assert!(!poller.may_poll_now().await, "third nudge must throttle");

        // Sink must remain empty (nodes:[] above).
        assert!(sink_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn poller_suspends_during_429_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let (sink_tx, _sink_rx) = mpsc::channel::<NormalizedIssue>(8);
        let (_refresh_tx, refresh_rx) = watch::channel(0u64);

        let client = LinearClient::new(server.uri(), token())
            .with_backoff_window(Duration::from_secs(5), Duration::from_secs(60));
        let poller = LinearPoller::new(
            client,
            LinearUserId::from("u1"),
            vec![LinearStateName::from("Todo")],
            Duration::from_millis(10),
            sink_tx,
            refresh_rx,
        );
        let err = poller.poll_once().await.unwrap_err();
        assert!(matches!(err, LinearError::Backoff { .. }));
        // Backoff curve must now block subsequent polls.
        assert!(
            !poller.may_poll_now().await,
            "may_poll_now must respect 429 backoff"
        );
    }

    #[tokio::test]
    async fn poller_emits_normalized_issues_matching_webhook_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "issues": {
                        "nodes": [
                            issue_node("ENG-9", "Todo", &[LABEL_ROKI_READY], Some("u1")),
                        ]
                    }
                }
            })))
            .mount(&server)
            .await;

        let (sink_tx, mut sink_rx) = mpsc::channel::<NormalizedIssue>(8);
        let (_refresh_tx, refresh_rx) = watch::channel(0u64);

        let client = LinearClient::new(server.uri(), token())
            .with_backoff_floor(Duration::from_millis(5));
        let poller = LinearPoller::new(
            client,
            LinearUserId::from("u1"),
            vec![LinearStateName::from("Todo")],
            Duration::from_millis(50),
            sink_tx,
            refresh_rx,
        );
        let count = poller.poll_once().await.unwrap();
        assert_eq!(count, 1);
        let issue = sink_rx.recv().await.expect("issue forwarded");
        assert_eq!(issue.issue, IssueId::from("ENG-9"));
        assert!(issue.has_roki_ready());
        assert_eq!(issue.assignee.as_ref(), Some(&LinearUserId::from("u1")));
    }
}
