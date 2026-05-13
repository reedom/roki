//! E2E: rule's post phase exits non-zero (process_crash). A matching
//! [[on_failure]] handler runs and emits {"directive":"end"}, so the daemon
//! exits 0. Events file has exactly one cycle_completed event with
//! cycle_kind=failure and zero failure_unhandled events. Both the failed rule
//! cycle's iter dir and the handler cycle's iter dir exist on disk.

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
async fn on_failure_handler_recovers_from_process_crash() {
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

    let ticket_id = "ENG-200";

    let workflow_path = work.path().join("WORKFLOW.yaml");
    // post0 SIGKILLs itself → daemon-detected ProcessCrash. The on_failure
    // handler matches when.kind = "process_crash"; its terminal task writes
    // a directive file with `outcome: "handled"`.
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
        run: 'true'
      - id: post0
        run: 'kill -KILL $$'

on_failure:
  - when:
      kind: process_crash
    tasks:
      - id: frun0
        run: 'echo handled'
      - id: fpost0
        run: 'printf ''{"directive":"end","outcome":"handled"}'' > "$ROKI_DIRECTIVE_PATH"'
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
        .post(&webhook_url)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    // Events file: exactly one cycle_completed with cycle_kind=failure;
    // zero failure_unhandled events.
    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    wait_for_event_count(&events_path, "cycle_completed", 1, Duration::from_secs(15)).await;
    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0), "binary should exit 0 after SIGTERM");

    let body = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|e| panic!("events.jsonl must exist at {events_path:?}: {e}"));
    let lines: Vec<&str> = body.lines().collect();

    assert_eq!(
        lines.len(),
        1,
        "expected exactly 1 event (cycle_completed for the handler); got:\n{body}"
    );
    assert!(
        lines[0].contains("\"event\":\"cycle_completed\""),
        "line 0 must be cycle_completed: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("\"cycle_kind\":\"failure\""),
        "line 0 must have cycle_kind=failure: {}",
        lines[0]
    );
    assert!(
        !body.contains("\"event\":\"failure_unhandled\""),
        "no failure_unhandled events expected; got:\n{body}"
    );

    // Slice 7: when [[on_failure]] succeeds, no escalation_added is emitted.
    let daemon_events_path = session_root.join("_daemon.events.jsonl");
    if daemon_events_path.exists() {
        let daemon_body = std::fs::read_to_string(&daemon_events_path).unwrap_or_default();
        assert!(
            !daemon_body.contains("\"event\":\"escalation_added\""),
            "no escalation_added expected when [[on_failure]] succeeds:\n{daemon_body}"
        );
    }

    // Both the failed rule cycle's iter dir and the handler cycle's iter dir
    // must exist. Look for two distinct cycle-<uuid> dirs under the ticket dir.
    let ticket_dir = session_root.join(ticket_id);
    assert!(
        ticket_dir.exists(),
        "ticket dir must exist at {ticket_dir:?}"
    );

    let cycle_dirs: Vec<_> = std::fs::read_dir(&ticket_dir)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .collect();

    assert_eq!(
        cycle_dirs.len(),
        2,
        "expected 2 cycle dirs (failed rule + handler); found {}; dirs: {:?}",
        cycle_dirs.len(),
        cycle_dirs.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );

    for entry in &cycle_dirs {
        let visit_dir = entry.path().join("visit-1");
        assert!(
            visit_dir.exists(),
            "visit-1 must exist under {:?}",
            entry.path()
        );
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
        "timed out waiting for {expected} occurrences of {event_kind} in {}",
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
