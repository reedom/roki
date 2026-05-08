//! E2E: persistent daemon runs two cycles for the same ticket without
//! exiting in between. Webhook A (status=todo) → cycle 1; webhook B
//! (status=in_progress) after cycle 1 ends → cycle 2; SIGTERM → exit 0.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn persistent_runs_two_cycles_for_same_ticket() {
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

    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    let ticket_id = "ENG-100";

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

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "true"
[rule.post]
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"in_progress_done\"}'"
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

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::new();

    let payload_todo = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": ticket_id,
            "assignee": {"id": "u1"},
            "state": {"name": "todo"},
            "labels": []
        }
    });
    let resp = client
        .post(&webhook_url)
        .json(&payload_todo)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    wait_for_event_count(&events_path, "cycle_completed", 1, Duration::from_secs(15)).await;

    let payload_in_progress = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": ticket_id,
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });
    let resp = client
        .post(&webhook_url)
        .json(&payload_in_progress)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    wait_for_event_count(&events_path, "cycle_completed", 2, Duration::from_secs(15)).await;

    // SIGTERM and wait for clean exit.
    sigterm_child(&child);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s after SIGTERM")
        .expect("child wait succeeds");
    assert!(
        status.success(),
        "binary should exit 0 after SIGTERM, got {status:?}"
    );

    let body = std::fs::read_to_string(&events_path).unwrap();
    let cycle_completed_count = body.matches("\"event\":\"cycle_completed\"").count();
    assert_eq!(
        cycle_completed_count, 2,
        "expected exactly 2 cycle_completed events; got:\n{body}"
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

async fn wait_for_event_count(
    path: &std::path::Path,
    event_kind: &str,
    expected_count: usize,
    timeout: Duration,
) {
    let needle = format!("\"event\":\"{event_kind}\"");
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(body) = tokio::fs::read_to_string(path).await {
            if body.matches(&needle).count() >= expected_count {
                return;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "timed out waiting for {expected_count} occurrences of {event_kind} in {}",
        path.display()
    );
}

fn sigterm_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}
