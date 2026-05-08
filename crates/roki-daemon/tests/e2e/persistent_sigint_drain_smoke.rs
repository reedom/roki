//! E2E: SIGINT during an in-flight cycle drains within shutdown_window_seconds.
//!
//! Sequence:
//!   1. Spawn daemon, wait for webhook listener.
//!   2. POST webhook (status=in_progress) for ENG-100 to start a 300ms cycle.
//!   3. Sleep 100ms — cycle should be in the run-phase sleep.
//!   4. Send SIGINT to the daemon process.
//!   5. Wait for `daemon_shutdown_began` in `_daemon.events.jsonl`.
//!   6. Wait for `daemon_shutdown_completed`.
//!   7. Wait for child exit; assert exit code 0.
//!
//! Assertions:
//!   - `daemon_shutdown_began` has `signal == "sigint"`.
//!   - `daemon_shutdown_completed` has `aborted == 0`.
//!   - No `shutdown_window_exceeded` event in the daemon log.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn sigint_drains_in_flight_cycle_within_window() {
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
    // Single rule: matches in_progress; run sleeps 300ms so we can SIGINT
    // while the cycle is in-flight.
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
cmd = "sh -c 'sleep 0.3'"
[rule.post]
cmd = "printf '{\"directive\":\"end\"}'"
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
shutdown_window_seconds = 5

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

    // POST webhook to start an in_progress cycle (run phase sleeps 300ms).
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
    let resp = client
        .post(&webhook_url)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    // Sleep 100ms — the cycle should be in its run-phase sleep by now.
    sleep(Duration::from_millis(100)).await;

    // Send SIGINT to trigger graceful shutdown while the cycle is in-flight.
    sigint_child(&child);

    let daemon_events_path = session_root.join("_daemon.events.jsonl");

    // Wait for daemon_shutdown_began (timeout 5s).
    let began_event = wait_for_event_kind(
        &daemon_events_path,
        "daemon_shutdown_began",
        Duration::from_secs(5),
    )
    .await;

    // Wait for daemon_shutdown_completed (timeout 10s).
    let completed_event = wait_for_event_kind(
        &daemon_events_path,
        "daemon_shutdown_completed",
        Duration::from_secs(10),
    )
    .await;

    // Wait for child to exit cleanly (timeout 10s).
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("binary should exit within 10s after SIGINT")
        .expect("child wait succeeds");
    assert!(
        status.success(),
        "binary should exit 0 after SIGINT drain, got {status:?}"
    );

    // Assert daemon_shutdown_began carries signal == "sigint".
    assert_eq!(
        began_event["signal"].as_str(),
        Some("sigint"),
        "daemon_shutdown_began must carry signal=sigint; got: {began_event}"
    );

    // Assert daemon_shutdown_completed carries aborted == 0.
    assert_eq!(
        began_event["signal"].as_str().unwrap_or(""),
        "sigint",
        "signal field must be sigint"
    );
    let aborted = completed_event["aborted"].as_u64().unwrap_or_else(|| {
        panic!("missing aborted in daemon_shutdown_completed: {completed_event}")
    });
    assert_eq!(
        aborted, 0,
        "daemon_shutdown_completed must have aborted=0; got: {completed_event}"
    );

    // Assert no shutdown_window_exceeded event exists.
    let body = std::fs::read_to_string(&daemon_events_path).unwrap_or_default();
    assert!(
        !body.contains("\"event\":\"shutdown_window_exceeded\""),
        "shutdown_window_exceeded must NOT appear in daemon log;\n{body}"
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

/// Polls `path` until a line with `"event":"<kind>"` appears, then returns
/// the parsed JSON value for that line. Panics if `timeout` expires first.
async fn wait_for_event_kind(
    path: &std::path::Path,
    kind: &str,
    timeout: Duration,
) -> serde_json::Value {
    let needle = format!("\"event\":\"{kind}\"");
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(body) = tokio::fs::read_to_string(path).await {
            for line in body.lines() {
                if line.contains(&needle) {
                    return serde_json::from_str(line)
                        .unwrap_or_else(|e| panic!("parse event line: {e}\nline: {line}"));
                }
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for event {kind:?} in {}", path.display());
}

fn sigint_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGINT);
    }
}
