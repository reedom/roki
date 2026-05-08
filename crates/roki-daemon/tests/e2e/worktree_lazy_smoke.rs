//! E2E: worktree is materialized lazily on first pre->run, reused across
//! cycles, and recreated when an out-of-band removal happens between cycles.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn worktree_lazy_create_reuse_recreate() {
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

    let work = TempDir::new().unwrap();
    let session_root = work.path().join("sessions");
    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::create_dir_all(&wt_root).unwrap();

    let ticket_id = "OPS-200";

    let workflow_path = work.path().join("WORKFLOW.toml");
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
[rule.pre]
cmd = "printf '{\"directive\":\"run\"}'"
[rule.run]
cmd = "pwd > $ROKI_ITER_DIR/cwd_capture.txt"
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

    // ---------- Cycle 1: worktree absent at start ----------
    let binary = env!("CARGO_BIN_EXE_roki");
    let spawn_one = || {
        Command::new(binary)
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
            .unwrap()
    };

    assert!(
        !wt_root.join(ticket_id).exists(),
        "precondition: no worktree"
    );

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));

    let mut child = spawn_one();
    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;
    post_webhook(port, ticket_id).await;

    wait_for_event_count(&events_path, "cycle_completed", 1, Duration::from_secs(15)).await;
    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0), "cycle 1 must exit 0 after SIGTERM");
    assert!(
        wt_root.join(ticket_id).is_dir(),
        "worktree must be created in cycle 1"
    );

    // ---------- Cycle 2: same ticket; worktree must be reused (still on disk) ----------
    let mut child = spawn_one();
    wait_for_listener(webhook_addr).await;
    post_webhook(port, ticket_id).await;
    // Each spawn writes to the same per-ticket events file, so wait for the
    // cumulative count to reach 2.
    wait_for_event_count(&events_path, "cycle_completed", 2, Duration::from_secs(15)).await;
    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0), "cycle 2 must exit 0 after SIGTERM");
    assert!(
        wt_root.join(ticket_id).is_dir(),
        "worktree must still exist after cycle 2"
    );

    // ---------- Cycle 3: out-of-band remove; ensure must recreate ----------
    std::fs::remove_dir_all(wt_root.join(ticket_id)).unwrap();
    assert!(!wt_root.join(ticket_id).exists());

    let mut child = spawn_one();
    wait_for_listener(webhook_addr).await;
    post_webhook(port, ticket_id).await;
    wait_for_event_count(&events_path, "cycle_completed", 3, Duration::from_secs(15)).await;
    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0), "cycle 3 must exit 0 after SIGTERM");
    assert!(
        wt_root.join(ticket_id).is_dir(),
        "worktree must be recreated by cycle 3"
    );
}

async fn post_webhook(port: u16, ticket_id: &str) {
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
        .post(format!("http://127.0.0.1:{port}/"))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
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
