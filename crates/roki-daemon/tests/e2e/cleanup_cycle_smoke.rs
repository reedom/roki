//! E2E: a non-shorthand [[cleanup]] entry runs as a cleanup cycle, then
//! post_cycle_delete removes the ticket dir. Two events emitted in order.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cleanup_cycle_runs_then_deletes() {
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

    let ticket_id = "ENG-100";

    let workflow_path = work.path().join("WORKFLOW.toml");
    let workflow_body = format!(
        r#"
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

[[cleanup]]
[cleanup.when]
status = "done"
[cleanup.when.labels]
has_all = []
[cleanup.run]
cmd = "echo cleanup-run"
[cleanup.post]
cmd = "printf '{{\"directive\":\"end\",\"outcome\":\"cleanup_done\"}}'"
"#
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
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn roki binary");

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;

    let webhook_url = format!("http://127.0.0.1:{port}/");
    // status "done" matches the cleanup entry's when.status="done" and does
    // NOT match the rule's when.status="in_progress", so dispatch picks cleanup.
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
    let client = reqwest::Client::new();
    let resp = client.post(&webhook_url).json(&payload).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s")
        .expect("child wait succeeds");
    assert!(status.success(), "binary should exit 0, got {status:?}");

    let ticket_dir = session_root.join(ticket_id);
    assert!(
        !ticket_dir.exists(),
        "ticket dir should be deleted after cleanup cycle; remains at {ticket_dir:?}"
    );

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let body = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|e| panic!("events.jsonl must exist at {events_path:?}: {e}"));
    let lines: Vec<&str> = body.lines().collect();

    assert_eq!(
        lines.len(),
        2,
        "expected 2 events: cycle_completed + worktree_delete_requested; got:\n{body}"
    );
    assert!(
        lines[0].contains("\"event\":\"cycle_completed\""),
        "line 0 must be cycle_completed: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("\"cycle_kind\":\"cleanup\""),
        "line 0 must have cycle_kind=cleanup: {}",
        lines[0]
    );
    let iters_one = lines[0].contains("\"iters\":1");
    let iters_more = lines[0].contains("\"iters\":2") || lines[0].contains("\"iters\":3");
    assert!(
        iters_one || iters_more,
        "iters should be >= 1; got {}",
        lines[0]
    );
    assert!(
        lines[1].contains("\"event\":\"worktree_delete_requested\""),
        "line 1 must be worktree_delete_requested: {}",
        lines[1]
    );
    assert!(
        lines[1].contains("\"reason\":\"cleanup_terminal\""),
        "line 1 must have reason=cleanup_terminal: {}",
        lines[1]
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
