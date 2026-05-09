//! E2E (slice 6): the webhook listener binds early and the
//! `ready_gate_layer` middleware parks every request with HTTP 503 until
//! the cold-start phase finishes and `daemon_ready` is emitted. Slice 6
//! Task 9 added the gate; this fixture is its end-to-end witness.
//!
//! The shape of the test is:
//!
//!  1. Mount a `viewer { id }` stub that answers immediately and an
//!     `issues(...)` enumerate stub that takes ~3 seconds. The slow
//!     enumerate keeps cold start "in progress" long enough for a
//!     well-timed webhook POST to land before `daemon_ready`.
//!  2. Boot `roki run`. Wait for the listener TCP socket to accept.
//!  3. POST a webhook *before* `daemon_ready` -> assert HTTP 503 with the
//!     `cold_start_in_progress` body the gate layer returns.
//!  4. Wait for `daemon_ready`.
//!  5. POST the same webhook again -> assert HTTP 2xx (the listener
//!     accepts and queues the dispatch).
//!  6. SIGTERM, exit 0.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::await_daemon_ready;

#[tokio::test]
async fn webhook_during_cold_start_returns_503() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral webhook port")
        .local_addr()
        .expect("local_addr")
        .port();

    let linear = MockServer::start().await;

    // Default-priority viewer stub. Fast.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"viewer": {"id": "u1"}}
        })))
        .mount(&linear)
        .await;

    // High-priority slow enumerate stub. ~3s delay buys us a wide window
    // where cold start is actively running and the gate is parked.
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(3))
                .set_body_json(serde_json::json!({
                    "data": {
                        "issues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                })),
        )
        .with_priority(1)
        .mount(&linear)
        .await;

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    let ticket_id = "ENG-700";

    let workflow_path = work.path().join("WORKFLOW.toml");
    let workflow_body = r#"
[admission]
assignee = "u1"

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
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"todo_done\"}'"
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
shutdown_window_seconds = 10

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
        .env("ROKI_WT_ROOT_OVERRIDE", &wt_root)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn roki binary");

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;

    // Cold start is mid-flight (the slow `issues(...)` stub holds for
    // ~3s). The listener is bound and accepting, but the gate layer
    // intercepts before the handler. Body content is irrelevant — the
    // gate runs before HMAC verification.
    let webhook_url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::new();

    let payload = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": ticket_id,
            "assignee": {"id": "u1"},
            "state": {"name": "todo"},
            "labels": []
        }
    });

    let early = client
        .post(&webhook_url)
        .json(&payload)
        .send()
        .await
        .expect("early webhook POST");
    assert_eq!(
        early.status().as_u16(),
        503,
        "expected 503 during cold start, got {}",
        early.status()
    );
    let early_body = early.text().await.unwrap_or_default();
    assert!(
        early_body.contains("cold_start_in_progress"),
        "expected 503 body to contain 'cold_start_in_progress', got: {early_body}"
    );

    // Wait for the gate to open.
    let _ = await_daemon_ready(&session_root).await;

    // Same payload, gate now open: handler accepts and replies 2xx.
    let late = client
        .post(&webhook_url)
        .json(&payload)
        .send()
        .await
        .expect("late webhook POST");
    let late_status = late.status();
    assert!(
        late_status.is_success(),
        "expected 2xx after daemon_ready, got {late_status}"
    );

    // Clean shutdown.
    sigterm_child(&child);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s after SIGTERM")
        .expect("child wait succeeds");
    assert!(
        status.success(),
        "binary should exit 0 after SIGTERM, got {status:?}"
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

fn sigterm_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}
