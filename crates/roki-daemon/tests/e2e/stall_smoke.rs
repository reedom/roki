//! E2E: a state subprocess that ignores SIGTERM and emits no stdout triggers
//! stall detection. With no `on_failure:` entry the per-ticket task emits
//! failure_unhandled; daemon stays alive; the test SIGTERMs it within the
//! stall window + grace period and asserts exit 0.

use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_ready, stub_empty_issues};

#[tokio::test]
async fn run_phase_stall_terminates_within_grace() {
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

    // Inline shell that ignores SIGTERM and sleeps. The watchdog must escalate
    // to SIGKILL after the 5 s grace period. `sleep` reads from /dev/null and
    // writes to /dev/null so it does not inherit the stdout pipe — otherwise
    // sleep would keep the pipe open after sh is SIGKILLed and the daemon's
    // tee_stdout reader would block until sleep exits naturally.
    // YAML double-quoted scalar; escape any embedded `\` and `"` if added.
    let run_cmd = r#"trap '' TERM; sleep 30 </dev/null >/dev/null 2>&1"#;

    let workflow_path = work.path().join("WORKFLOW.yaml");
    let workflow_body = format!(
        r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo

rules:
  - when:
      status: in_progress
    tasks:
      - id: run0
        run: "{run_cmd}"
"#,
        run_cmd = run_cmd,
    );
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
cli = "true"
stall_seconds = 1

[engine]
max_iterations = 1

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

    let started = Instant::now();
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
            "id": "ENG-STALL",
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

    let events_path = session_root.join("ENG-STALL.events.jsonl");
    wait_for_event_count(
        &events_path,
        "failure_unhandled",
        1,
        Duration::from_secs(15),
    )
    .await;
    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0), "binary should exit 0 after SIGTERM");
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "stall + grace + SIGTERM drain must finish well under 25 s, took {:?}",
        started.elapsed()
    );

    // Capture preserved on disk.
    let cycle_root = session_root.join("ENG-STALL");
    let cycle_entry = std::fs::read_dir(&cycle_root)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .expect("cycle-<uuid> dir present");
    let cycle_path = cycle_entry.path();
    let visit_dir = cycle_path.join("visit-1");
    assert!(
        visit_dir.join("run0.stdout").is_file(),
        "run0.stdout preserved"
    );
    assert!(
        !visit_dir.join("run0.terminal.json").exists(),
        "run0.terminal.json must not exist (no result event emitted)"
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
