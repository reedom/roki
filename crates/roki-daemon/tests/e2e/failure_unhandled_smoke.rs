//! E2E: rule's post phase exits non-zero (process_crash). No [[on_failure]]
//! entries exist, so the per-ticket task emits exactly one failure_unhandled
//! event with marker=none, cycle_kind=rule, failure.kind=process_crash. The
//! persistent daemon stays alive after the unhandled cycle; the test SIGTERMs
//! it and asserts exit 0 (a per-ticket failure does not propagate to the
//! daemon's exit code in slice 5+).

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
async fn rule_failure_with_no_handler_emits_failure_unhandled() {
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

    let ticket_id = "ENG-400";

    let workflow_path = work.path().join("WORKFLOW.toml");
    // The post phase exits non-zero with no JSON output → ProcessCrash. No
    // [[on_failure]] block, so route() returns None and the runtime emits
    // failure_unhandled with marker=none.
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
cmd = "true"
[rule.post]
cmd = "exit 7"
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

    // Events file: exactly one failure_unhandled with marker=none,
    // cycle_kind=rule, failure.kind=process_crash.
    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    wait_for_event_count(
        &events_path,
        "failure_unhandled",
        1,
        Duration::from_secs(15),
    )
    .await;
    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0), "binary should exit 0 after SIGTERM");

    let body = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|e| panic!("events.jsonl must exist at {events_path:?}: {e}"));
    let lines: Vec<&str> = body.lines().collect();

    assert_eq!(
        lines.len(),
        1,
        "expected exactly 1 event (failure_unhandled); got:\n{body}"
    );
    assert!(
        lines[0].contains("\"event\":\"failure_unhandled\""),
        "line 0 must be failure_unhandled: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("\"marker\":\"none\""),
        "line 0 must have marker=none: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("\"cycle_kind\":\"rule\""),
        "line 0 must have cycle_kind=rule: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("\"kind\":\"process_crash\""),
        "line 0 failure.kind must be process_crash: {}",
        lines[0]
    );

    // Slice 7: marker=none path is the only remaining failure_unhandled
    // emitter. No escalation_added must be emitted on the daemon log.
    let daemon_events_path = session_root.join("_daemon.events.jsonl");
    if daemon_events_path.exists() {
        let daemon_body = std::fs::read_to_string(&daemon_events_path).unwrap_or_default();
        assert!(
            !daemon_body.contains("\"event\":\"escalation_added\""),
            "no escalation_added expected for marker=none path:\n{daemon_body}"
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
