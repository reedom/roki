//! E2E (slice 6): cold start retries after a 429 with Retry-After:1
//! and emits linear_backoff_applied.

mod support_cold_start;

use std::net::TcpListener;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use support_cold_start::{await_cold_start_completed, await_daemon_event, issue_node};

#[tokio::test]
async fn cold_start_handles_429_with_retry_after() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral webhook port")
        .local_addr()
        .expect("local_addr")
        .port();

    let linear = MockServer::start().await;

    Mock::given(method("POST"))
        .and(body_string_contains("viewer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"viewer": {"id": "u1"}}
        })))
        .mount(&linear)
        .await;

    // First issues call: 429 with Retry-After: 1
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&linear)
        .await;

    // Subsequent issues call: 200 with one ticket
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [ issue_node("a1", "TEAM-1", "todo", "u1") ]
                }
            }
        })))
        .with_priority(2)
        .mount(&linear)
        .await;

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let workflow_path = work.path().join("WORKFLOW.yaml");
    std::fs::write(
        &workflow_path,
        r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/example/repo"

[[rule]]
[rule.when]
status = "todo"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "true"
[rule.post]
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"done\"}'"
"#,
    )
    .unwrap();

    let roki_path = work.path().join("roki.toml");
    std::fs::write(
        &roki_path,
        format!(
            r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = {port}

[default]
cli = "echo"

[engine]
max_iterations = 5
shutdown_window_seconds = 10

[paths]
workflow = "{}"
session_root = "{}"

[log]
"#,
            workflow_path.display(),
            session_root.display()
        ),
    )
    .unwrap();

    let binary = env!("CARGO_BIN_EXE_roki");
    let mut child = Command::new(binary)
        .arg("run")
        .arg("--config")
        .arg(&roki_path)
        .env("ROKI_LINEAR_GRAPHQL_URL", linear.uri())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn roki");

    let _backoff = await_daemon_event(
        &session_root,
        "linear_backoff_applied",
        Duration::from_secs(15),
    )
    .await;
    let report = await_cold_start_completed(&session_root).await;
    assert_eq!(report["enumerated"], 1, "report = {report}");
    assert_eq!(report["enum_partial"], false);

    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let pid = child.id().expect("child pid") as i32;
    let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    let _ = child.wait().await;
}
