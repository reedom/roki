//! E2E: cleanup cycle evicts the cache for ENG-100, then a second webhook for
//! the same ticket spawns a fresh ticket task and runs a rule cycle.
//!
//! Sequence:
//!   1. POST webhook A (status=done) → cleanup cycle runs, ticket dir deleted.
//!   2. POST webhook B (status=in_progress) → rule cycle runs in fresh ticket task.
//!   3. SIGTERM → daemon exits 0.
//!
//! Events asserted (multiset):
//!   line 0: cycle_completed cycle_kind=cleanup
//!   line 1: worktree_delete_requested reason=cleanup_terminal
//!   line 2: cycle_completed cycle_kind=rule

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cleanup_eviction_then_readmit_spawns_fresh_ticket_task() {
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

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    let ticket_id = "ENG-100";

    let workflow_path = work.path().join("WORKFLOW.toml");
    // Both [[cleanup]] and [[rule]] blocks are present.
    // cleanup matches status="done"; rule matches status="in_progress".
    // Each has a post.cmd that emits a terminal directive so the cycle ends.
    let workflow_body = r#"
[admission]
assignee = "u1"

[[admission.repos]]
ghq = "github.com/example/repo"

[[cleanup]]
[cleanup.when]
status = "done"
[cleanup.when.labels]
has_all = []
[cleanup.run]
cmd = "true"
[cleanup.post]
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"cleanup_done\"}'"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "true"
[rule.post]
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"rule_done\"}'"
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

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::new();

    // Webhook A: status=done → dispatches cleanup cycle.
    let payload_done = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": ticket_id,
            "assignee": {"id": "u1"},
            "state": {"name": "done"},
            "labels": []
        }
    });
    let resp = client
        .post(&webhook_url)
        .json(&payload_done)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));

    // Wait for the cleanup cycle_completed event.
    wait_for_event_count(&events_path, "cycle_completed", 1, Duration::from_secs(15)).await;

    // Wait for worktree_delete_requested — proof cleanup ran end-to-end and
    // triggered the post-cycle delete.
    wait_for_event_count(
        &events_path,
        "worktree_delete_requested",
        1,
        Duration::from_secs(15),
    )
    .await;

    // The ticket dir must be gone after the cleanup cycle.
    let ticket_dir = session_root.join(ticket_id);
    assert!(
        !ticket_dir.exists(),
        "ticket dir must be deleted after cleanup cycle; still present at {ticket_dir:?}"
    );

    // Webhook B: status=in_progress → dispatches rule cycle in a fresh ticket task.
    let payload_in_progress = serde_json::json!({
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
        .json(&payload_in_progress)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    // Wait for the second cycle_completed (the rule cycle).
    wait_for_event_count(&events_path, "cycle_completed", 2, Duration::from_secs(15)).await;

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

    // Assert the event sequence. The events.jsonl sibling survives the cleanup
    // cycle deletion because it lives at <session_root>/ENG-100.events.jsonl,
    // not inside the ticket dir.
    let body = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<&str> = body.lines().collect();

    // We expect at least 3 events: cycle_completed(cleanup), worktree_delete_requested,
    // cycle_completed(rule). Daemon lifecycle events may also be present.
    let cycle_completed_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains("\"event\":\"cycle_completed\""))
        .collect();
    assert_eq!(
        cycle_completed_lines.len(),
        2,
        "expected exactly 2 cycle_completed events; full log:\n{body}"
    );

    let wdr_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains("\"event\":\"worktree_delete_requested\""))
        .collect();
    assert_eq!(
        wdr_lines.len(),
        1,
        "expected exactly 1 worktree_delete_requested event; full log:\n{body}"
    );

    // Find positions of the three key events to verify ordering.
    let pos_cleanup_cycle = lines
        .iter()
        .position(|l| {
            l.contains("\"event\":\"cycle_completed\"") && l.contains("\"cycle_kind\":\"cleanup\"")
        })
        .expect("cleanup cycle_completed must exist");

    let pos_wdr = lines
        .iter()
        .position(|l| l.contains("\"event\":\"worktree_delete_requested\""))
        .expect("worktree_delete_requested must exist");

    let pos_rule_cycle = lines
        .iter()
        .position(|l| {
            l.contains("\"event\":\"cycle_completed\"") && l.contains("\"cycle_kind\":\"rule\"")
        })
        .expect("rule cycle_completed must exist");

    assert!(
        pos_cleanup_cycle < pos_wdr,
        "cleanup cycle_completed (line {pos_cleanup_cycle}) must precede \
         worktree_delete_requested (line {pos_wdr})"
    );
    assert!(
        pos_wdr < pos_rule_cycle,
        "worktree_delete_requested (line {pos_wdr}) must precede \
         rule cycle_completed (line {pos_rule_cycle})"
    );

    assert!(
        lines[pos_wdr].contains("\"reason\":\"cleanup_terminal\""),
        "worktree_delete_requested must carry reason=cleanup_terminal: {}",
        lines[pos_wdr]
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

fn sigterm_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}
