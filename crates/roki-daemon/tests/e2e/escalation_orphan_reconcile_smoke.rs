//! E2E: cold-start orphan reconcile fs error pushes a cycle-less
//! escalation_added entry to the daemon-scoped event log. Because the
//! orphan dir cannot be removed (it has mode 0o000, preventing descend),
//! `OrphanReport::fs_errors` carries one (ticket_id, io::Error) pair,
//! and `cold_start` invokes `escalation.push_daemon` once.

use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_ready, stub_empty_issues};

#[tokio::test]
async fn orphan_reconcile_fs_error_pushes_cycle_less_escalation() {
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
    std::fs::create_dir_all(&session_root).unwrap();

    // Pre-create an orphan dir that the reconcile step will try to delete.
    let orphan_id = "ORPHAN-1";
    let orphan_dir = session_root.join(orphan_id);
    std::fs::create_dir_all(&orphan_dir).unwrap();

    let workflow_path = work.path().join("WORKFLOW.yaml");
    let workflow_body = r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo
"#;
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let roki_path = work.path().join("roki.toml");
    let roki_body = format!(
        r#"
[linear]
token = "lin"

[linear.webhook]
bind = "127.0.0.1"
port = {port}

[default]
cli = "echo"

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

    // Remove all permissions from the orphan dir itself so remove_dir_all
    // fails to descend into it. This populates OrphanReport::fs_errors.
    let mut perms = std::fs::metadata(&orphan_dir).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&orphan_dir, perms).unwrap();

    let binary = env!("CARGO_BIN_EXE_roki");
    let mut child = Command::new(binary)
        .arg("run")
        .arg("--config")
        .arg(&roki_path)
        .env("ROKI_LINEAR_GRAPHQL_URL", linear.uri())
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn roki binary");

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;

    let daemon_events_path = session_root.join("_daemon.events.jsonl");
    wait_for_event_count(
        &daemon_events_path,
        "escalation_added",
        1,
        Duration::from_secs(15),
    )
    .await;

    // Restore writable permissions so test cleanup (TempDir drop) works.
    let mut restored = std::fs::metadata(&orphan_dir).unwrap().permissions();
    restored.set_mode(0o755);
    std::fs::set_permissions(&orphan_dir, restored).unwrap();

    let _ = await_daemon_ready(&session_root).await;

    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0));

    let body = std::fs::read_to_string(&daemon_events_path).unwrap();

    let escalations: Vec<&str> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"escalation_added\""))
        .collect();
    assert_eq!(escalations.len(), 1, "exactly one escalation_added: {body}");

    let line = escalations[0];
    assert!(
        !line.contains("\"ticket_id\""),
        "cycle-less entry must omit ticket_id: {line}"
    );
    assert!(
        !line.contains("\"cycle_id\""),
        "cycle-less entry must omit cycle_id: {line}"
    );
    assert!(line.contains("\"kind\":\"fs_poison\""), "{line}");

    assert!(
        body.contains("\"event\":\"cold_start_completed\""),
        "cold_start_completed should still fire: {body}"
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
