//! Slice 10 e2e: verify that `CycleSummary::last_state_id` is populated and
//! served by `GET /api/tickets/{id}/cycles`.
//!
//! Workflow layout: two non-terminal states `first` → `post0`, terminating at
//! the implicit `__success__` terminal. Because the daemon stores non-terminal
//! state IDs in alphabetical (BTreeMap) order in `cycle.json::states`, and
//! `"first" < "post0"` lexicographically, `states = ["first", "post0"]` and
//! `last_state_id = "post0"`.
//!
//! Spec fr:10 §`GET /api/tickets/{id}/cycles`, slice-10 `last_state_id`.

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
async fn cycles_last_state_id_is_populated_after_two_state_rule() {
    let webhook_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let api_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let linear = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"viewer": {"id": "u1"}}
        })))
        .mount(&linear)
        .await;
    stub_empty_issues(&linear).await;

    let work = TempDir::new().unwrap();
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    let ticket_id = "ENG-910";

    // Two-state sugar rule: `first` chains into `post0`, which exits 0 and
    // transitions to the implicit `__success__` terminal.
    //
    // BTreeMap key order: "first" < "post0" (f < p), so
    //   cycle.json::states = ["first", "post0"]
    //   → last_state_id = "post0"
    let workflow_path = work.path().join("WORKFLOW.yaml");
    let workflow_body = r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo

rules:
  - when:
      status: in_progress
    tasks:
      - id: first
        run: 'exit 0'
      - id: post0
        run: 'exit 0'
"#;
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let roki_path = work.path().join("roki.toml");
    let roki_body = format!(
        r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = {webhook_port}

[default.ai]
cli = "echo"

[engine]

[paths]
workflow = "{workflow}"
session_root = "{session_root}"

[log]

[api]
bind = "127.0.0.1"
port = {api_port}
"#,
        webhook_port = webhook_port,
        api_port = api_port,
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

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], webhook_port).into();
    wait_for_listener(webhook_addr).await;
    let _ = await_daemon_ready(&session_root).await;
    let api_addr: SocketAddr = ([127, 0, 0, 1], api_port).into();
    wait_for_listener(api_addr).await;

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
        .post(format!("http://127.0.0.1:{webhook_port}/"))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    wait_for_event_count(&events_path, "cycle_completed", 1, Duration::from_secs(20)).await;

    // GET /api/tickets/{id}/cycles — expect exactly one cycle.
    let resp = client
        .get(format!(
            "http://127.0.0.1:{api_port}/api/tickets/{ticket_id}/cycles"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body_text = resp.text().await.unwrap();

    // Raw JSON must contain the last_state_id field with value "post0".
    assert!(
        body_text.contains("\"last_state_id\":\"post0\""),
        "raw JSON must contain last_state_id=post0, got: {body_text}"
    );

    let cycles_json: serde_json::Value =
        serde_json::from_str(&body_text).expect("response must be valid JSON");

    let cycles = cycles_json["cycles"]
        .as_array()
        .expect("response must have a cycles array");
    assert_eq!(cycles.len(), 1, "expected exactly one cycle, got: {cycles_json}");

    let cycle = &cycles[0];

    // terminal_id: the implicit __success__ terminal (both `first` and `post0`
    // are non-terminal states so the cycle lands at __success__).
    assert_eq!(
        cycle["terminal_id"].as_str(),
        Some("__success__"),
        "terminal_id must be __success__, got: {cycle}"
    );

    // last_state_id: last non-terminal state visited, "post0" (alphabetically
    // last in BTreeMap keys ["first", "post0"]).
    assert_eq!(
        cycle["last_state_id"].as_str(),
        Some("post0"),
        "last_state_id must be post0, got: {cycle}"
    );

    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0));
}

async fn wait_for_listener(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("listener never came up at {addr}");
}

async fn wait_for_event_count(
    path: &std::path::Path,
    event_kind: &str,
    expected: usize,
    timeout: Duration,
) {
    let needle = format!("\"event\":\"{event_kind}\"");
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(body) = tokio::fs::read_to_string(path).await {
            if body.matches(&needle).count() >= expected {
                return;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "timed out waiting for {expected} {event_kind} in {}",
        path.display()
    );
}

async fn sigterm_and_wait(child: &mut tokio::process::Child, timeout: Duration) -> Option<i32> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status.code(),
        _ => None,
    }
}
