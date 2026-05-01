//! Integration tests for the Linear tracker adapter (task 2.5).
//!
//! These tests stand up a `wiremock` server that pretends to be Linear's
//! GraphQL endpoint and exercise the polling loop end-to-end:
//!
//! * `cadence_cap_respects_configured_interval` proves the polling cadence is
//!   not violated under steady load (Requirement 3.2).
//! * `http_429_defers_next_request_to_same_endpoint` proves a 429 response
//!   schedules the next request after the advertised `Retry-After` window
//!   (Requirement 3.3).
//! * `valid_payload_normalizes_to_normalized_issue` proves the GraphQL
//!   response is normalized into the `NormalizedIssue` shape (Requirement 3.4).

use std::sync::Arc;
use std::time::{Duration, Instant};

use roki_daemon::config::SecretString;
use roki_daemon::orchestrator::state::RepoId;
use roki_daemon::tools::NoopRateLimit;
use roki_daemon::tracker::linear::{LinearTracker, LinearTrackerConfig, ScopeWatch};
use roki_daemon::tracker::model::IssueState;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::sleep;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TEST_TOKEN: &str = "lin_api_test_super_secret_value";

fn payload_with_two_issues() -> Value {
    json!({
        "data": {
            "issues": {
                "nodes": [
                    {
                        "id": "uuid-1",
                        "identifier": "ENG-1",
                        "title": "First issue",
                        "description": "the first issue body",
                        "state": { "type": "started", "name": "In Progress" },
                        "labels": { "nodes": [{ "name": "bug" }, { "name": "p1" }] },
                        "team": { "key": "ENG" }
                    },
                    {
                        "id": "uuid-2",
                        "identifier": "ENG-2",
                        "title": "Second issue",
                        "description": "the second issue body",
                        "state": { "type": "completed", "name": "Done" },
                        "labels": { "nodes": [] },
                        "team": { "key": "ENG" }
                    }
                ]
            }
        }
    })
}

fn empty_payload() -> Value {
    json!({ "data": { "issues": { "nodes": [] } } })
}

fn scope_watch() -> ScopeWatch {
    // TODO(7.1c): drop this scaffolding when the single workspace tracker
    // collapses. Post-7.1a `ScopeWatch` carries only the repo stamp.
    ScopeWatch {
        repo: RepoId::new("core"),
    }
}

#[tokio::test]
async fn cadence_cap_respects_configured_interval() {
    // Cadence: 1 second per scope. Run the loop ~3.3 seconds.
    // Expectation: the wiremock server receives at most 4 requests
    // (t=0, t=1, t=2, t=3). Anything significantly above that means the
    // cadence cap is being violated under steady load.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_payload()))
        .mount(&server)
        .await;

    let endpoint = format!("{}/graphql", server.uri());
    let cadence = Duration::from_millis(1000);
    let config = LinearTrackerConfig {
        endpoint: endpoint.clone(),
        cadence,
        scopes: vec![scope_watch()],
        token: SecretString::new(TEST_TOKEN),
        rate_limit: Arc::new(NoopRateLimit),
    };

    let (tx, _rx) = mpsc::channel(16);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let tracker = LinearTracker::new(config);

    let handle = tokio::spawn(async move { tracker.run(tx, shutdown_rx).await });

    // Run for ~3.3s to allow at most 4 polls (immediate + 1s + 2s + 3s).
    sleep(Duration::from_millis(3300)).await;
    let _ = shutdown_tx.send(());
    let _ = handle.await.expect("tracker task joins");

    let calls = server.received_requests().await.unwrap_or_default().len();
    assert!(
        calls <= 4,
        "tracker exceeded cadence cap: observed {calls} requests in 3.3s for a 1s cadence",
    );
    assert!(
        2 <= calls,
        "tracker did not poll often enough: observed {calls} requests in 3.3s for a 1s cadence",
    );
}

#[tokio::test]
async fn http_429_defers_next_request_to_same_endpoint() {
    // First response: 429 with Retry-After: 2 seconds.
    // Second response: 200 OK with empty payload.
    // We assert that the gap between the two requests respects the
    // Retry-After advertisement (Requirement 3.3).
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "2")
                .set_body_json(json!({ "errors": [{ "message": "rate limited" }] })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_payload()))
        .mount(&server)
        .await;

    // Tight cadence so we know the gap we observe is the backoff, not the
    // cadence interval.
    let endpoint = format!("{}/graphql", server.uri());
    let config = LinearTrackerConfig {
        endpoint,
        cadence: Duration::from_millis(100),
        scopes: vec![scope_watch()],
        token: SecretString::new(TEST_TOKEN),
        rate_limit: Arc::new(NoopRateLimit),
    };

    let (tx, _rx) = mpsc::channel(16);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let tracker = LinearTracker::new(config);
    let started = Instant::now();
    let handle = tokio::spawn(async move { tracker.run(tx, shutdown_rx).await });

    // Wait until the server has received at least 2 requests (or the test
    // times out at 5 seconds).
    let deadline = started + Duration::from_secs(5);
    loop {
        let len = server.received_requests().await.unwrap_or_default().len();
        if 2 <= len {
            break;
        }
        if deadline < Instant::now() {
            let _ = shutdown_tx.send(());
            let _ = handle.await;
            panic!("tracker never made the second request after the 429");
        }
        sleep(Duration::from_millis(50)).await;
    }
    let received = server.received_requests().await.unwrap();
    let _ = shutdown_tx.send(());
    let _ = handle.await.expect("tracker task joins");

    let first = received[0].headers.get("authorization");
    let second = received[1].headers.get("authorization");
    assert!(first.is_some(), "first request must have auth header");
    assert!(second.is_some(), "second request must have auth header");

    // The actual gap: we don't have per-request timestamps from wiremock,
    // but the elapsed between t=0 and the moment the second request was
    // observed must be at least the Retry-After value (2s) minus a small
    // tolerance.
    let elapsed = started.elapsed();
    assert!(
        Duration::from_millis(1800) <= elapsed,
        "second request fired too early after the 429 (elapsed={elapsed:?})",
    );
}

#[tokio::test]
async fn valid_payload_normalizes_to_normalized_issue() {
    // Stub a valid 200 GraphQL response and assert the tracker emits a
    // NormalizedIssue with the documented fields populated. This pins
    // Requirement 3.4 (normalized issue model includes id, title,
    // description, state, label set, team identifier).
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(payload_with_two_issues()))
        .mount(&server)
        .await;

    let endpoint = format!("{}/graphql", server.uri());
    let config = LinearTrackerConfig {
        endpoint,
        cadence: Duration::from_millis(50),
        scopes: vec![scope_watch()],
        token: SecretString::new(TEST_TOKEN),
        rate_limit: Arc::new(NoopRateLimit),
    };

    let (tx, mut rx) = mpsc::channel(16);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let tracker = LinearTracker::new(config);
    let handle = tokio::spawn(async move { tracker.run(tx, shutdown_rx).await });

    // Collect the first two emitted NormalizedIssue events.
    let first = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("tracker emits first issue within timeout")
        .expect("channel is open");
    let second = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("tracker emits second issue within timeout")
        .expect("channel is open");

    let _ = shutdown_tx.send(());
    let _ = handle.await.expect("tracker task joins");

    // Use a hashmap-style lookup since the iteration order from `nodes` is
    // preserved but we want robust assertions.
    let issues = [first, second];
    let by_id: std::collections::HashMap<&str, &roki_daemon::tracker::model::NormalizedIssue> =
        issues.iter().map(|i| (i.issue.as_str(), i)).collect();

    let one = by_id
        .get("ENG-1")
        .expect("ENG-1 emitted as a NormalizedIssue");
    assert_eq!(one.title, "First issue");
    assert_eq!(one.description, "the first issue body");
    assert_eq!(one.state, IssueState::Active);
    assert_eq!(one.labels, vec!["bug".to_string(), "p1".to_string()]);
    assert_eq!(one.team_or_scope, "ENG");
    assert_eq!(one.repo.as_str(), "core");

    let two = by_id
        .get("ENG-2")
        .expect("ENG-2 emitted as a NormalizedIssue");
    assert_eq!(two.state, IssueState::Terminal);
    assert!(two.labels.is_empty());
}

#[tokio::test]
async fn graphql_request_carries_authorization_header_and_query_body() {
    // Defense in depth around Requirement 3.5: the daemon must send only
    // read queries (the active-issue query) and must use the daemon-owned
    // token. We assert by inspecting the raw request body.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty_payload()))
        .mount(&server)
        .await;

    let endpoint = format!("{}/graphql", server.uri());
    let config = LinearTrackerConfig {
        endpoint,
        cadence: Duration::from_millis(50),
        scopes: vec![scope_watch()],
        token: SecretString::new(TEST_TOKEN),
        rate_limit: Arc::new(NoopRateLimit),
    };

    let (tx, _rx) = mpsc::channel(16);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let tracker = LinearTracker::new(config);
    let handle = tokio::spawn(async move { tracker.run(tx, shutdown_rx).await });

    // Wait for at least one request.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let len = server.received_requests().await.unwrap_or_default().len();
        if 1 <= len {
            break;
        }
        if deadline < Instant::now() {
            let _ = shutdown_tx.send(());
            let _ = handle.await;
            panic!("tracker never issued a request");
        }
        sleep(Duration::from_millis(20)).await;
    }
    let requests = server.received_requests().await.unwrap();
    let _ = shutdown_tx.send(());
    let _ = handle.await.expect("tracker joins");

    let req: &Request = &requests[0];
    let auth = req
        .headers
        .get("authorization")
        .expect("authorization header present");
    assert_eq!(auth.to_str().unwrap(), TEST_TOKEN);

    let body: Value = serde_json::from_slice(&req.body).expect("body is JSON");
    let query = body
        .get("query")
        .and_then(Value::as_str)
        .expect("body has `query`");
    // The query targets Linear's `issues` field with a team filter; we don't
    // pin the exact text but require it to be a query operation that names
    // the resource and the team scope.
    assert!(
        query.contains("issues") && query.contains("team"),
        "query should target the issues resource scoped by team; got: {query}",
    );
}
