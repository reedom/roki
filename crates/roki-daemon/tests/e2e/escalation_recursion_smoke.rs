//! E2E: rule's post phase exits non-zero (process_crash). The on_failure
//! handler's post phase also exits non-zero. The recursion bound prevents a
//! second handler cycle; the per-ticket task pushes a single
//! escalation_added entry to the daemon-scoped event log instead of emitting
//! failure_unhandled. The persistent daemon stays alive afterwards; the test
//! SIGTERMs it and asserts exit 0.

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
async fn recursion_bound_pushes_escalation_added() {
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

    let ticket_id = "ENG-300";

    let workflow_path = work.path().join("WORKFLOW.yaml");
    // rule's post0 SIGKILLs → ProcessCrash. on_failure matches; the handler's
    // fpost0 also SIGKILLs → second ProcessCrash inside a Failure cycle.
    // Recursion bound fires; daemon emits escalation_added marker=recursion_bound.
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
        run: 'true'
      - id: fpost0
        run: 'kill -KILL $$'
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

    // fr:06: handler-cycle-fails route through the escalation queue, not
    // failure_unhandled. The persistent daemon stays alive.
    let daemon_events_path = session_root.join("_daemon.events.jsonl");
    wait_for_event_count(
        &daemon_events_path,
        "escalation_added",
        1,
        Duration::from_secs(15),
    )
    .await;
    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0), "binary should exit 0 after SIGTERM");

    let body = std::fs::read_to_string(&daemon_events_path)
        .unwrap_or_else(|e| panic!("_daemon events.jsonl must exist: {e}"));

    let escalations: Vec<&str> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"escalation_added\""))
        .collect();
    assert_eq!(
        escalations.len(),
        1,
        "expected exactly one escalation_added; got:\n{body}"
    );

    let unhandled: Vec<&str> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"failure_unhandled\""))
        .collect();
    assert!(
        unhandled.is_empty(),
        "no failure_unhandled expected on daemon log:\n{body}"
    );

    let entry = escalations[0];
    assert!(entry.contains("\"ticket_id\":"), "{entry}");
    assert!(entry.contains("\"cycle_id\":"), "{entry}");
    assert!(entry.contains("\"kind\":"), "{entry}");
    assert!(entry.contains("\"state_id\":"), "{entry}");

    // Per-ticket events file: should contain NO failure_unhandled and NO
    // escalation_added (escalation_added lands on the daemon-scoped log only).
    let per_ticket_events = session_root.join(format!("{ticket_id}.events.jsonl"));
    if per_ticket_events.exists() {
        let pt_body = std::fs::read_to_string(&per_ticket_events).unwrap_or_default();
        assert!(
            !pt_body.contains("\"event\":\"failure_unhandled\""),
            "per-ticket file must not contain failure_unhandled:\n{pt_body}"
        );
        assert!(
            !pt_body.contains("\"event\":\"escalation_added\""),
            "per-ticket file must not contain escalation_added:\n{pt_body}"
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
