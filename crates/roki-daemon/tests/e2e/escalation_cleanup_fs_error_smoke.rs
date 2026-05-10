//! E2E: cleanup-time `wt remove` failure pushes to the escalation queue
//! (fr:06) and emits `escalation_added` on the daemon-scoped events file.
//! The persistent daemon stays alive afterwards; the test SIGTERMs it and
//! asserts exit 0.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_ready, stub_empty_issues};

#[tokio::test]
async fn cleanup_wt_remove_failure_pushes_escalation() {
    let port = TcpListener::bind("127.0.0.1:0")
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
    let registry = work.path().join("wt-registry");
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::create_dir_all(&registry).unwrap();

    let ticket_id = "OPS-500";

    // Pre-populate the fake registry so `wt list` reports the worktree
    // present, ensuring `worktree::remove` actually attempts `wt remove`.
    std::fs::create_dir_all(registry.join(ticket_id)).unwrap();
    // Pre-create the session tempdir so cleanup has something to delete after.
    std::fs::create_dir_all(session_root.join(ticket_id)).unwrap();

    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/fixtures/wt_fail_remove.sh");
    assert!(fixture.is_file(), "fixture script missing: {fixture:?}");

    let workflow_path = work.path().join("WORKFLOW.yaml");
    let workflow_body = r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo

cleanup:
  - when:
      status: done
    tasks:
      - id: crun0
        run: 'true'
      - id: cpost0
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

[default.ai]
cli = "echo"

[engine]
max_iterations = 3

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
        .env("ROKI_WT_BIN_OVERRIDE", &fixture)
        .env("ROKI_WT_FAKE_REGISTRY", &registry)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;
    // Slice 6: cold start runs after the listener binds. Wait for
    // `daemon_ready` so the gate is open and the POST below is not
    // short-circuited to 503 `cold_start_in_progress`.
    let _ = await_daemon_ready(&session_root).await;

    let payload = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": ticket_id,
            "assignee": {"id": "u1"},
            "state": {"name": "done"},
            "labels": []
        }
    });
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/"))
        .json(&payload)
        .send()
        .await
        .unwrap();

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

    let body = std::fs::read_to_string(&daemon_events_path).unwrap();
    assert!(
        !body.contains("\"event\":\"failure_unhandled\""),
        "no failure_unhandled expected:\n{body}"
    );
    assert!(
        !body.contains("\"cycle_kind\":\"failure\""),
        "no failure-handler cycle expected (cleanup_fs_error must skip [[on_failure]]):\n{body}"
    );
    let escalations: Vec<_> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"escalation_added\""))
        .collect();
    assert_eq!(
        escalations.len(),
        1,
        "exactly one escalation_added:\n{body}"
    );
    let line = escalations[0];
    assert!(line.contains("\"kind\":\"fs_poison\""), "{line}");
    assert!(line.contains("\"state_id\":\"post\""), "{line}");
    assert!(line.contains("\"ticket_id\":"), "{line}");
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
