//! E2E: cross-ticket cycles run concurrently. Two tickets, each runs
//! a 500ms run phase. Cycles overlap in wall-clock time.
//!
//! Overlap proof strategy: the daemon emits `cycle_completed` (with `ts`)
//! but no `cycle_started` event. However, since both run phases sleep 500ms,
//! if both cycles ran sequentially the second completion would be at least
//! 500ms after the first. We assert the gap between completions is strictly
//! less than 500ms — proving they executed concurrently.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_ready, stub_empty_issues};

#[tokio::test]
async fn cross_ticket_cycles_overlap() {
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

    let workflow_path = work.path().join("WORKFLOW.yaml");
    // Single rule: matches in_progress; run sleeps 500ms so two concurrent
    // cycles (ENG-100 and ENG-200) both spend time in run phase simultaneously.
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
        run: 'sh -c ''sleep 0.5'''
      - id: post0
        run: 'printf ''{\"directive\":\"end\"}'''
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

[default]
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
    // Slice 6: cold start runs after the listener binds. Wait for
    // `daemon_ready` so the gate is open and the POST below is not
    // short-circuited to 503 `cold_start_in_progress`.
    let _ = await_daemon_ready(&session_root).await;

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::new();

    // POST both webhooks back-to-back without awaiting between them so the
    // daemon receives both before either cycle finishes its 500ms run phase.
    let payload_eng100 = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": "ENG-100",
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });
    let payload_eng200 = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": "ENG-200",
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });

    let resp_a = client
        .post(&webhook_url)
        .json(&payload_eng100)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_a.status().as_u16(), 202);

    let resp_b = client
        .post(&webhook_url)
        .json(&payload_eng200)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_b.status().as_u16(), 202);

    let events_path_a = session_root.join("ENG-100.events.jsonl");
    let events_path_b = session_root.join("ENG-200.events.jsonl");

    // Wait for both tickets to reach cycle_completed.
    wait_for_event_count(
        &events_path_a,
        "cycle_completed",
        1,
        Duration::from_secs(30),
    )
    .await;
    wait_for_event_count(
        &events_path_b,
        "cycle_completed",
        1,
        Duration::from_secs(30),
    )
    .await;

    // Read the completion timestamps to prove temporal overlap.
    // The daemon emits `cycle_completed` but no `cycle_started` event, so we
    // prove concurrency indirectly: each run phase sleeps 500ms. If the cycles
    // ran sequentially the gap between their completions would be >= 500ms.
    // A gap < 500ms proves both were in their run phase at the same time.
    let completed_a = read_first_event_ts(&events_path_a, "cycle_completed");
    let completed_b = read_first_event_ts(&events_path_b, "cycle_completed");

    let gap = if completed_a > completed_b {
        completed_a - completed_b
    } else {
        completed_b - completed_a
    };
    // Allow up to 450ms tolerance (run=500ms, so serial gap would be >=500ms).
    let run_duration = time::Duration::milliseconds(500);
    assert!(
        gap < run_duration,
        "cycles appear to have run sequentially (gap >= run duration):\n  \
         ENG-100 cycle_completed={completed_a}\n  \
         ENG-200 cycle_completed={completed_b}\n  \
         gap={gap:?}, run_duration={run_duration:?}"
    );

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

/// Reads `path`, finds the first JSONL line whose `event` field matches
/// `event_kind`, parses the `ts` field as RFC3339, and returns it.
fn read_first_event_ts(path: &std::path::Path, event_kind: &str) -> OffsetDateTime {
    let body =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("parse JSONL line: {e}\nline: {line}"));
        if v["event"].as_str() == Some(event_kind) {
            let ts_str = v["ts"]
                .as_str()
                .unwrap_or_else(|| panic!("missing ts in line: {line}"));
            return OffsetDateTime::parse(ts_str, &Rfc3339)
                .unwrap_or_else(|e| panic!("parse ts={ts_str}: {e}"));
        }
    }
    panic!("event {} not found in {}", event_kind, path.display());
}

fn sigterm_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}
