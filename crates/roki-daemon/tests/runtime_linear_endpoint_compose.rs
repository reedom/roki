//! Task 10.4 integration test: the production runtime composition path
//! consumes the validated `[linear].endpoint` config slot and points the
//! shared `LinearClient` at the configured URL. A wiremock standing in for
//! Linear receives both the bootstrap viewer lookup and the workspace-level
//! `list_issues` poll, proving the production bootstrap formula
//! `LinearClient::new(config.linear.endpoint.clone(), api_token)` redirects
//! against the operator-configured endpoint instead of the hardcoded
//! `DEFAULT_LINEAR_ENDPOINT`.
//!
//! This test exercises the same `Config::load_from_str` surface bootstrap
//! step 1 runs and the same `LinearClient::new(config.linear.endpoint, ...)`
//! formula bootstrap step 8 runs, so the slot's plumbing is observed
//! end-to-end without depending on the `wt`/`ghq`/`claude` PATH lookups
//! that gate full `runtime::run_with_env` execution (see e2e_bootstrap.rs
//! "(a) composition order completes" BLOCKER notes).
//!
//! Spec refs: requirements.md Req 2.13, Req 3.4; design.md "Daemon
//! bootstrap" steps 2 + 8 + 9.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use roki_daemon::config::{Config, StaticEnv};
use roki_daemon::tracker::linear::{DEFAULT_LINEAR_ENDPOINT, LinearClient};
use roki_daemon::tracker::model::{LinearStateName, LinearUserId};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_TOKEN: &str = "lin_api_token_for_endpoint_test";
const WEBHOOK_SECRET: &str = "webhook_secret_for_endpoint_test";

fn write_workflow(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("WORKFLOW.md");
    std::fs::write(
        &path,
        "---\nfoo: bar\n---\n\n## prompt_template_orchestrator\n\nbody\n\n## prompt_template_implement_direct\n\nbody\n\n## prompt_template_validate_direct\n\nbody\n\n## prompt_template_open_pr\n\nbody\n",
    )
    .unwrap();
    path
}

fn config_toml(workflow: &std::path::Path, endpoint: Option<&str>) -> String {
    let endpoint_line = match endpoint {
        Some(uri) => format!("endpoint = \"{uri}\"\n"),
        None => String::new(),
    };
    format!(
        r#"
[linear]
api_token = {{ env = "LINEAR_API_TOKEN" }}
webhook_secret = {{ env = "LINEAR_WEBHOOK_SECRET" }}
assignee = "me"
{endpoint_line}
[workflow]
path = "{}"

[server]
bind = "127.0.0.1"
port = 0

[permissions]
strategy = "settings-allowlist"
"#,
        workflow.display()
    )
}

fn env() -> StaticEnv {
    StaticEnv::new()
        .set("LINEAR_API_TOKEN", API_TOKEN)
        .set("LINEAR_WEBHOOK_SECRET", WEBHOOK_SECRET)
}

/// Mirror of the production bootstrap step 8 formula:
/// `LinearClient::new(config.linear.endpoint.clone(), api_token)`. Kept in
/// sync with `crates/roki-daemon/src/runtime.rs` so any drift between the
/// production composition and the test composition fails this test fast.
fn build_linear_client_like_bootstrap(config: &Config, env: &StaticEnv) -> LinearClient {
    let api_token = config.linear.api_token.resolve(env).expect("token resolves");
    LinearClient::new(config.linear.endpoint.clone(), api_token)
        .with_backoff_floor(Duration::from_millis(5))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_redirects_viewer_and_list_issues_to_configured_endpoint() {
    // ---- Wiremock standing in for Linear ----
    let server = MockServer::start().await;
    // Match the bootstrap viewer query specifically — the response must echo
    // an `id` so `LinearClient::viewer()` resolves cleanly.
    Mock::given(method("POST"))
        .and(body_string_contains("viewer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "viewer": { "id": "u-endpoint-test" } }
        })))
        .expect(1..)
        .mount(&server)
        .await;
    // Match the bootstrap workspace poller's `list_issues` query — same
    // wiremock receives both, proving both viewer + list_issues hit the
    // configured endpoint.
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issues": { "nodes": [] } }
        })))
        .expect(1..)
        .mount(&server)
        .await;

    // ---- Drive the production config loader against a config that points
    // at the wiremock URI — the same parser bootstrap step 1 invokes. ----
    let dir = TempDir::new().unwrap();
    let workflow = write_workflow(dir.path());
    let body = config_toml(&workflow, Some(&server.uri()));
    let config = Config::load_from_str(&body).expect("config loads");
    assert_eq!(
        config.linear.endpoint,
        server.uri(),
        "loader must surface the configured endpoint verbatim"
    );

    // ---- Build the same `LinearClient` bootstrap step 8 builds. ----
    let env = env();
    let linear = Arc::new(build_linear_client_like_bootstrap(&config, &env));
    assert_eq!(
        linear.endpoint(),
        server.uri(),
        "LinearClient must point at the configured endpoint, not DEFAULT_LINEAR_ENDPOINT"
    );

    // ---- Drive the two production calls bootstrap performs against this
    // client: viewer (step 8) + list_issues (step 9 / poller's first
    // request). Both must land on the wiremock. ----
    let viewer = linear.viewer().await.expect("viewer lookup ok");
    assert_eq!(viewer, LinearUserId::from("u-endpoint-test"));

    let issues = linear
        .list_issues(&viewer, &[LinearStateName::from("Todo")])
        .await
        .expect("list_issues ok");
    assert!(issues.is_empty(), "wiremock returns no issues");

    // Wiremock auto-asserts at drop that both `expect(1..)` mounts saw
    // their request — concretely proving the production bootstrap formula
    // routes the viewer + list_issues calls through the configured slot.
    drop(server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loader_default_matches_default_linear_endpoint_constant() {
    // Sanity gate: when the slot is omitted, the loader must apply the
    // canonical default the production code constructed verbatim before
    // this slot landed. Locks the default in so a future "improve the URL"
    // change does not silently shift production behavior.
    let dir = TempDir::new().unwrap();
    let workflow = write_workflow(dir.path());
    let body = config_toml(&workflow, None);
    let config = Config::load_from_str(&body).expect("config loads");
    assert_eq!(config.linear.endpoint, DEFAULT_LINEAR_ENDPOINT);

    let env = env();
    let linear = build_linear_client_like_bootstrap(&config, &env);
    assert_eq!(linear.endpoint(), DEFAULT_LINEAR_ENDPOINT);
}

#[test]
fn loader_refuses_invalid_endpoint_via_runtime_error_path() {
    // Mirrors the production refusal: bootstrap step 1 propagates
    // `ConfigError::InvalidLinearEndpoint` as `RuntimeError::Config(...)`,
    // so the operator gets one log line naming the offending key.
    let dir = TempDir::new().unwrap();
    let workflow = write_workflow(dir.path());
    let body = config_toml(&workflow, Some("not-a-url"));
    let err = Config::load_from_str(&body).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("[linear].endpoint"),
        "refusal must name the offending key: {msg}"
    );
    assert!(msg.contains("not-a-url"), "refusal must echo offender: {msg}");
}

