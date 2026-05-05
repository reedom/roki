//! Task 10.1.4 integration test: runtime composition spawns a single
//! workspace-level `LinearTracker` poller, exposes a `TrackerHandle` carrying
//! the `TrackerRefresh` nudge endpoint, and stops the poller cleanly under
//! shutdown.
//!
//! Asserts the runtime composition layer (not the poller's internal request
//! shape — those are exercised by `tests/integration_tracker.rs` and the
//! in-file unit tests under `tracker::linear`). The seam used here is
//! `runtime::testing::compose_poller_for_test`, which mirrors the production
//! `bootstrap` step that constructs + spawns the poller against a wiremock'd
//! Linear endpoint with a sub-second cadence floor and a shrunk backoff curve.
//!
//! Spec refs: requirements.md Req 3.3, 3.4, 13.3; design.md "Daemon bootstrap"
//! step 9.

use std::sync::Arc;
use std::time::Duration;

use roki_daemon::config::SecretValue;
use roki_daemon::runtime::testing::{PollerHarness, compose_poller_for_test};
use roki_daemon::shutdown;
use roki_daemon::tracker::linear::LinearClient;
use roki_daemon::tracker::model::{IssueId, LABEL_ROKI_READY, LinearStateName, LinearUserId};
use roki_daemon::tracker::refresh::{NudgeResult, TrackerRefresh};
use tokio::sync::mpsc;
use tokio::time::Instant;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn issue_node(id: &str, state: &str, labels: &[&str], assignee: Option<&str>) -> serde_json::Value {
    let label_nodes: Vec<serde_json::Value> = labels
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    let assignee = match assignee {
        Some(id) => serde_json::json!({ "id": id }),
        None => serde_json::Value::Null,
    };
    serde_json::json!({
        "identifier": id,
        "title": "title",
        "description": "body",
        "state": { "name": state },
        "labels": { "nodes": label_nodes },
        "assignee": assignee,
    })
}

/// Build a `LinearClient` pointed at the supplied wiremock URI with the
/// shrunk backoff curve every test in this file uses.
fn test_client(uri: &str, backoff_floor: Duration, backoff_cap: Duration) -> LinearClient {
    LinearClient::new(uri.to_owned(), SecretValue::new("tok"))
        .with_backoff_window(backoff_floor, backoff_cap)
}

/// (a) Poll emits a `NormalizedIssue` for an admit-states + assignee match
/// within the cadence window. The poll loop runs continuously; the test
/// reads the first emission off the shared `issue_rx` and exits.
#[tokio::test]
async fn poller_emits_normalized_issue_for_admit_match_within_cadence_window() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "issues": {
                    "nodes": [
                        issue_node("ENG-101", "Todo", &[LABEL_ROKI_READY], Some("viewer-1")),
                    ]
                }
            }
        })))
        .mount(&server)
        .await;

    let (issue_tx, mut issue_rx) = mpsc::channel(8);
    let client = test_client(
        &server.uri(),
        Duration::from_millis(5),
        Duration::from_secs(1),
    );
    let (signal, trigger) = shutdown::new();

    let harness: PollerHarness = compose_poller_for_test(
        client,
        LinearUserId::from("viewer-1"),
        vec![LinearStateName::from("Todo")],
        Duration::from_millis(40),
        issue_tx,
        signal.clone(),
    )
    .expect("poller harness compose");

    let issue = tokio::time::timeout(Duration::from_secs(2), issue_rx.recv())
        .await
        .expect("poller emitted an issue within the cadence window")
        .expect("issue_rx open");
    assert_eq!(issue.issue, IssueId::from("ENG-101"));
    assert!(issue.has_roki_ready());
    assert_eq!(
        issue.assignee.as_ref(),
        Some(&LinearUserId::from("viewer-1"))
    );

    // Tear down so the test does not leak the spawned task.
    trigger.fire();
    let _ = tokio::time::timeout(Duration::from_secs(2), harness.poller_join).await;
}

/// (b) A 429 response suspends polling for the documented backoff window.
/// The harness's `backoff` handle exposes the shared deadline that the
/// production runtime uses; after the 429 lands, the deadline must move
/// forward by at least the configured floor.
#[tokio::test]
async fn poller_suspends_after_429_for_documented_backoff_window() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let (issue_tx, _issue_rx) = mpsc::channel(8);
    let backoff_floor = Duration::from_millis(80);
    let client = test_client(&server.uri(), backoff_floor, Duration::from_secs(60));
    let (signal, trigger) = shutdown::new();

    let started = Instant::now();
    let harness: PollerHarness = compose_poller_for_test(
        client,
        LinearUserId::from("viewer-1"),
        vec![LinearStateName::from("Todo")],
        Duration::from_millis(20),
        issue_tx,
        signal.clone(),
    )
    .expect("poller harness compose");

    // Wait long enough for the first poll attempt to happen and the 429 to
    // land in the shared backoff state.
    let mut deadline = harness.backoff.next_allowed().await;
    let mut waited = Duration::ZERO;
    while deadline <= started && waited < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
        deadline = harness.backoff.next_allowed().await;
    }
    let suspended_for = deadline.saturating_duration_since(started);
    assert!(
        suspended_for >= backoff_floor,
        "after 429, the shared backoff deadline must move forward by at \
         least the floor ({backoff_floor:?}); saw {suspended_for:?}",
    );

    trigger.fire();
    let _ = tokio::time::timeout(Duration::from_secs(2), harness.poller_join).await;
}

/// (c) `TrackerRefresh::nudge` honours throttle / backoff per Task 3.5.
/// The nudge endpoint is the same handle the runtime publishes from the
/// composed `TrackerHandle`.
#[tokio::test]
async fn tracker_refresh_nudge_accepted_then_throttled_then_backoff_active() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issues": { "nodes": [] } }
        })))
        .mount(&server)
        .await;

    let (issue_tx, _issue_rx) = mpsc::channel(8);
    let cadence = Duration::from_millis(80);
    let client = test_client(
        &server.uri(),
        Duration::from_millis(5),
        Duration::from_secs(60),
    );
    let (signal, trigger) = shutdown::new();
    let harness: PollerHarness = compose_poller_for_test(
        client,
        LinearUserId::from("viewer-1"),
        vec![LinearStateName::from("Todo")],
        cadence,
        issue_tx,
        signal.clone(),
    )
    .expect("poller harness compose");

    // First nudge: accepted (cadence not yet observed).
    assert_eq!(harness.refresh.nudge(), NudgeResult::Accepted);
    // Second nudge within the cadence floor: throttled.
    assert_eq!(harness.refresh.nudge(), NudgeResult::Throttled);
    // Reset cadence + force backoff active; nudge must surface BackoffActive.
    tokio::time::sleep(cadence + Duration::from_millis(20)).await;
    harness
        .backoff
        .set_deadline_for_test(Instant::now() + Duration::from_secs(60));
    assert_eq!(harness.refresh.nudge(), NudgeResult::BackoffActive);

    trigger.fire();
    let _ = tokio::time::timeout(Duration::from_secs(2), harness.poller_join).await;
}

/// (d) Shutdown stops the poller within a bounded window.
#[tokio::test]
async fn shutdown_stops_poller_within_bounded_window() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issues": { "nodes": [] } }
        })))
        .mount(&server)
        .await;

    let (issue_tx, _issue_rx) = mpsc::channel(8);
    let client = test_client(
        &server.uri(),
        Duration::from_millis(5),
        Duration::from_secs(60),
    );
    let (signal, trigger) = shutdown::new();
    let harness: PollerHarness = compose_poller_for_test(
        client,
        LinearUserId::from("viewer-1"),
        vec![LinearStateName::from("Todo")],
        Duration::from_millis(40),
        issue_tx,
        signal.clone(),
    )
    .expect("poller harness compose");

    // Let one poll complete so the loop is firmly inside the select.
    tokio::time::sleep(Duration::from_millis(60)).await;
    let started = Instant::now();
    trigger.fire();
    let outcome = tokio::time::timeout(Duration::from_secs(2), harness.poller_join)
        .await
        .expect("poller join completed within bound");
    outcome.expect("poller task did not panic");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown must propagate to poller in well under SHUTDOWN_WINDOW"
    );
}

/// Smoke test: the production cadence floor is bounded below by 60 seconds
/// (per design.md "Daemon bootstrap" step 9 + Req 3.3); the config loader
/// must refuse `[linear].poll_cadence_seconds < 60` so a misconfigured
/// operator cannot accidentally hammer the Linear API.
#[test]
fn config_refuses_poll_cadence_below_minimum() {
    use roki_daemon::config::{Config, ConfigError};
    use std::path::PathBuf;
    let dir = tempfile::TempDir::new().unwrap();
    let workflow = dir.path().join("WORKFLOW.md");
    std::fs::write(&workflow, "stub").unwrap();
    let body = format!(
        r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"
poll_cadence_seconds = 30

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
        workflow.display()
    );
    let err = Config::load_from_str(&body).unwrap_err();
    assert!(
        matches!(err, ConfigError::PollCadenceTooLow { value: 30, .. }),
        "expected PollCadenceTooLow, got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("poll_cadence_seconds"), "{msg}");
    assert!(msg.contains("60"), "remediation must mention the floor: {msg}");
    let _: PathBuf = workflow; // silence unused-variable lint when assertions short-circuit
}

/// Anchor: the `Arc<dyn TrackerRefresh>` carried on the `TrackerHandle` is
/// cheap to clone so 10.1.5 / 10.1.6 / observability can fan it out without
/// re-constructing the underlying watcher.
#[tokio::test]
async fn tracker_refresh_handle_is_cloneable_arc() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issues": { "nodes": [] } }
        })))
        .mount(&server)
        .await;

    let (issue_tx, _issue_rx) = mpsc::channel(8);
    let client = test_client(
        &server.uri(),
        Duration::from_millis(5),
        Duration::from_secs(60),
    );
    let (signal, trigger) = shutdown::new();
    let harness: PollerHarness = compose_poller_for_test(
        client,
        LinearUserId::from("viewer-1"),
        vec![LinearStateName::from("Todo")],
        Duration::from_millis(40),
        issue_tx,
        signal.clone(),
    )
    .expect("poller harness compose");

    // Two clones of the same handle observe the same nudge cadence.
    let handle_a: Arc<dyn TrackerRefresh> = harness.refresh.clone();
    let handle_b: Arc<dyn TrackerRefresh> = harness.refresh.clone();
    assert_eq!(handle_a.nudge(), NudgeResult::Accepted);
    // The second handle is throttled because the first already consumed the
    // cadence window — proves both clones share state.
    assert_eq!(handle_b.nudge(), NudgeResult::Throttled);

    trigger.fire();
    let _ = tokio::time::timeout(Duration::from_secs(2), harness.poller_join).await;
}
