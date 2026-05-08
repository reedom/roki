//! E2E: `roki cleanup` subcommand only matches [[cleanup]] entries.
//!
//! Sends a webhook with status=in_progress. The workflow has a [[rule]] that
//! would match in default dispatch mode, but the `cleanup` subcommand forces
//! CleanupOnly dispatch — so [[rule]] is ignored. The [[cleanup]] entry
//! requires status=done, which does not match in_progress, so dispatch returns
//! NoMatch. The daemon keeps listening; we kill it after a short wait.
//! Assert: no events file and no ticket dir were created.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cleanup_subcommand_ignores_rule_list() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral webhook port")
        .local_addr()
        .expect("local_addr")
        .port();

    let linear = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"viewer": {"id": "u1"}}
        })))
        .mount(&linear)
        .await;

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let ticket_id = "ENG-600";

    let workflow_path = work.path().join("WORKFLOW.toml");
    let workflow_body = r#"
[admission]
assignee = "u1"

[[admission.repos]]
ghq = "github.com/example/repo"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "true"

[[cleanup]]
[cleanup.when]
status = "done"
[cleanup.when.labels]
has_all = []
[cleanup.run]
cmd = "true"
"#;
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let roki_path = work.path().join("roki.toml");
    let roki_body = format!(
        r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = {port}

[default.ai.command]
cli = "echo"

[engine]
max_iterations = 5

[paths]
workflow = "{workflow}"
session_root = "{session_root}"

[log]
"#,
        port = port,
        workflow = workflow_path.display(),
        session_root = session_root.display(),
    );
    std::fs::write(&roki_path, roki_body).unwrap();

    let binary = env!("CARGO_BIN_EXE_roki");
    let mut child = Command::new(binary)
        .arg("cleanup")
        .arg("--config")
        .arg(&roki_path)
        .env("ROKI_LINEAR_GRAPHQL_URL", linear.uri())
        .env("ROKI_GHQ_BASE_OVERRIDE", work.path())
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn roki binary");

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;

    // status=in_progress: [[rule]] would match under default dispatch,
    // but the cleanup subcommand ignores [[rule]]. [[cleanup]] requires
    // status=done, so dispatch returns NoMatch and the daemon keeps listening.
    let payload = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": ticket_id,
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/"))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    // Give the listener time to process the webhook and log no-match,
    // then kill the daemon (it would otherwise wait forever).
    sleep(Duration::from_millis(500)).await;
    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

    // No cycle ran: events file must not exist.
    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    assert!(
        !events_path.exists(),
        "expected no events file; found {events_path:?}"
    );

    // No cycle ran: ticket dir must not exist.
    let ticket_dir = session_root.join(ticket_id);
    assert!(
        !ticket_dir.exists(),
        "expected no ticket dir; found {ticket_dir:?}"
    );
}

async fn wait_for_listener(addr: SocketAddr) {
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("webhook listener never came up at {addr}");
}
