//! E2E: session_root is a regular file, not a directory. Every attempt to
//! create iter dirs (and the events file) underneath it fails with ENOTDIR,
//! which routes to FailureKind::FsPoison. The matching [[on_failure]] handler
//! is dispatched but its iter dir creation also fails (same broken root) →
//! recursion_bound. The daemon must exit non-zero.
//!
//! The events file cannot be written (session_root is a file), so no JSONL
//! assertions are made; only the process exit code is checked.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn fs_poison_routes_through_on_failure() {
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

    // Write a regular file at the path where session_root would be. The daemon
    // will try to open the events file at <session_root>/ENG-500.events.jsonl
    // and to create iter dirs at <session_root>/ENG-500/cycle-<uuid>/iter-1/.
    // Both fail because session_root is a file, not a directory.
    let session_root = work.path().join("sessions");
    std::fs::write(&session_root, b"not a dir").unwrap();
    assert!(session_root.is_file(), "pre-condition: sessions must be a file");

    let ticket_id = "ENG-500";

    let workflow_path = work.path().join("WORKFLOW.toml");
    let workflow_body = format!(
        r#"
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

[[on_failure]]
when.kind = "fs_poison"
[on_failure.run]
cmd = "true"
"#
    );
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
        .arg("run")
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

    let webhook_url = format!("http://127.0.0.1:{port}/");
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
    let resp = client.post(&webhook_url).json(&payload).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s")
        .expect("child wait succeeds");

    // FsPoison → on_failure handler dispatched → handler cycle also hits
    // FsPoison (same broken root) → recursion_bound → daemon exits non-zero.
    assert!(
        !status.success(),
        "binary should exit non-zero on fs_poison; got {status:?}"
    );
    // The events file cannot exist when session_root is a plain file;
    // no JSONL assertions are possible here.
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
