//! Integration tests for the `linear_graphql` proxy tool. The HTTP boundary
//! is stubbed with `wiremock` so we can drive the success path, the 429
//! rate-limit path, and the server-error path without reaching out to the
//! real Linear API.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use roki_daemon::config::SecretString;
use roki_daemon::tools::linear_graphql::LinearGraphqlTool;
use roki_daemon::tools::{NoopRateLimit, RateLimitState, RateLimited, Tool, ToolError};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "lin_api_test_super_secret_value";

#[tokio::test]
async fn forwards_single_operation_and_returns_unmodified_payload() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("authorization", TEST_TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "viewer": { "id": "user-1" } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tool = LinearGraphqlTool::new(
        format!("{}/", server.uri()),
        SecretString::new(TEST_TOKEN),
        Arc::new(NoopRateLimit),
    )
    .unwrap();

    let result = tool
        .call(json!({
            "query": "query Me { viewer { id } }",
            "variables": {}
        }))
        .await
        .unwrap();

    assert_eq!(result, json!({ "data": { "viewer": { "id": "user-1" } } }));
}

#[tokio::test]
async fn rate_limit_response_bubbles_up_and_updates_shared_state() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "13")
                .set_body_json(json!({ "errors": [{ "message": "rate limited" }] })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let tracker = Arc::new(RecordingRateLimit::default());
    let tool = LinearGraphqlTool::new(
        format!("{}/", server.uri()),
        SecretString::new(TEST_TOKEN),
        tracker.clone(),
    )
    .unwrap();

    let err = tool
        .call(json!({
            "query": "query Me { viewer { id } }",
            "variables": {}
        }))
        .await
        .unwrap_err();

    match err {
        ToolError::RateLimited {
            retry_after_seconds,
        } => {
            assert_eq!(retry_after_seconds, Some(13));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }

    assert_eq!(tracker.last_status.load(Ordering::SeqCst), 429);
    assert_eq!(tracker.last_retry_after.load(Ordering::SeqCst), 13);

    let rendered = format!("{err:?}");
    assert!(
        !rendered.contains(TEST_TOKEN),
        "error debug leaked the token: {rendered}"
    );
}

#[tokio::test]
async fn server_error_does_not_echo_the_token() {
    let server = MockServer::start().await;

    // Linear-style server error that helpfully echoes the auth header into
    // its diagnostic body — exactly the kind of thing redaction must catch.
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(500).set_body_string(format!(
            "internal error: received Authorization: {TEST_TOKEN}"
        )))
        .expect(1)
        .mount(&server)
        .await;

    let tool = LinearGraphqlTool::new(
        format!("{}/", server.uri()),
        SecretString::new(TEST_TOKEN),
        Arc::new(NoopRateLimit),
    )
    .unwrap();

    let err = tool
        .call(json!({
            "query": "query Me { viewer { id } }",
            "variables": {}
        }))
        .await
        .unwrap_err();

    let rendered = format!("{err}");
    let debug_rendered = format!("{err:?}");

    match err {
        ToolError::LinearHttpError { status } => assert_eq!(status, 500),
        other => panic!("expected LinearHttpError, got {other:?}"),
    }

    assert!(
        !rendered.contains(TEST_TOKEN),
        "Display leaked the token: {rendered}"
    );
    assert!(
        !debug_rendered.contains(TEST_TOKEN),
        "Debug leaked the token: {debug_rendered}"
    );
}

#[tokio::test]
async fn rejects_multi_operation_documents_before_any_http_call() {
    // No `Mock::given(...)` is mounted, so any HTTP attempt would surface as
    // an unmatched-request panic. We assert the proxy short-circuits.
    let server = MockServer::start().await;

    let tool = LinearGraphqlTool::new(
        format!("{}/", server.uri()),
        SecretString::new(TEST_TOKEN),
        Arc::new(NoopRateLimit),
    )
    .unwrap();

    let err = tool
        .call(json!({
            "query": "query A { viewer { id } } query B { viewer { id } }",
            "variables": {}
        }))
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::MultipleOperations));
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[derive(Default)]
struct RecordingRateLimit {
    last_status: AtomicU64,
    last_retry_after: AtomicU64,
}

#[async_trait]
impl RateLimitState for RecordingRateLimit {
    async fn before_call(&self) -> Result<(), RateLimited> {
        Ok(())
    }

    async fn record_response(&self, status: u16, retry_after: Option<u64>) {
        self.last_status.store(u64::from(status), Ordering::SeqCst);
        if let Some(value) = retry_after {
            self.last_retry_after.store(value, Ordering::SeqCst);
        }
    }
}
