//! E2E: SIGINT during a long in-flight cycle exceeds `shutdown_window_seconds`.
//!
//! Sequence:
//!   1. Spawn daemon, wait for webhook listener.
//!   2. POST webhook (status=in_progress) for ENG-100 to start a 30s cycle.
//!   3. Sleep 100ms — cycle should be in the run-phase sleep.
//!   4. Send SIGINT to the daemon process.
//!   5. Wait for `daemon_shutdown_began` in `_daemon.events.jsonl`.
//!   6. Wait for `shutdown_window_exceeded` (window is only 1s).
//!   7. Wait for child exit (≤ 8s); assert exit code is 1.
//!
//! Assertions:
//!   - `shutdown_window_exceeded` has `aborted >= 1`.
//!   - `shutdown_window_exceeded` carries `offenders[].ticket_id` containing "ENG-100".

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_ready, stub_empty_issues};

#[tokio::test]
async fn sigint_long_cycle_emits_shutdown_window_exceeded() {
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
    stub_empty_issues(&linear).await;

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    let ticket_id = "ENG-100";

    let workflow_path = work.path().join("WORKFLOW.yaml");
    // Single rule: matches in_progress; run ignores SIGTERM and sleeps 30s.
    // `trap '' TERM` makes the shell ignore SIGTERM so terminate_child_external's
    // 5s grace period elapses before SIGKILL, guaranteeing the 1s shutdown
    // window is exceeded before the ticket task's subprocess dies.
    let workflow_body = r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo

rules:
  - when:
      status: in_progress
    tasks:
      - id: run0
        run: 'sh -c ''trap """" TERM; sleep 30'''
      - id: post0
        run: 'printf ''{\"directive\":\"end\"}'''
"#;
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let roki_path = work.path().join("roki.toml");
    // shutdown_window_seconds = 1 is the smallest legal value (range [1, 600]).
    let roki_body = format!(
        r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = {port}

[default.ai]
cli = "echo"

[engine]
max_iterations = 5
shutdown_window_seconds = 1

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
    // Slice 6: cold start runs after the listener binds. Wait for
    // `daemon_ready` so the gate is open and the POST below is not
    // short-circuited to 503 `cold_start_in_progress`.
    let _ = await_daemon_ready(&session_root).await;

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::new();

    // POST webhook to start an in_progress cycle (run phase sleeps 30s).
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
    wait_for_event_kind(
        &daemon_events_path,
        "daemon_shutdown_began",
        Duration::from_secs(5),
    )
    .await;

    // Wait for shutdown_window_exceeded (timeout 8s — covers 1s drain + abort + cleanup).
    let exceeded_event = wait_for_event_kind(
        &daemon_events_path,
        "shutdown_window_exceeded",
        Duration::from_secs(8),
    )
    .await;

    // Wait for child to exit (timeout 8s); it must exit with code 1.
    let status = tokio::time::timeout(Duration::from_secs(8), child.wait())
        .await
        .expect("binary should exit within 8s after SIGINT timeout")
        .expect("child wait succeeds");
    assert_eq!(
        status.code(),
        Some(1),
        "binary must exit with code 1 (ShutdownWindowExceeded); got {status:?}"
    );

    // Assert shutdown_window_exceeded carries aborted >= 1.
    let aborted = exceeded_event["aborted"]
        .as_u64()
        .unwrap_or_else(|| panic!("missing aborted in shutdown_window_exceeded: {exceeded_event}"));
    assert!(
        aborted >= 1,
        "shutdown_window_exceeded must have aborted >= 1; got: {exceeded_event}"
    );

    let offenders = exceeded_event["offenders"].as_array().unwrap_or_else(|| {
        panic!("missing offenders in shutdown_window_exceeded: {exceeded_event}")
    });
    let ticket_ids: Vec<&str> = offenders
        .iter()
        .filter_map(|o| o["ticket_id"].as_str())
        .collect();
    assert!(
        ticket_ids.contains(&"ENG-100"),
        "offenders[].ticket_id must contain \"ENG-100\"; got: {exceeded_event}"
    );

    // SIGKILL must have fired during drain — the offender pid is dead by
    // the time the daemon process exits.
    let pids: Vec<i64> = offenders
        .iter()
        .filter_map(|o| o["pid"].as_u64())
        .map(|p| p as i64)
        .collect();
    assert!(
        !pids.is_empty(),
        "offenders[].pid must be present: {exceeded_event}"
    );
    for pid in pids {
        let pid_t = nix::unistd::Pid::from_raw(pid as i32);
        let mut alive = nix::sys::signal::kill(pid_t, None).is_ok();
        if alive {
            for _ in 0..40 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                if nix::sys::signal::kill(pid_t, None).is_err() {
                    alive = false;
                    break;
                }
            }
        }
        assert!(!alive, "offender pid {pid} should be dead at daemon exit");
    }
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
