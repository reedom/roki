//! E2E: a webhook for an admitted ticket that matches a `[[cleanup]]`
//! shorthand entry causes immediate deletion of `<session_root>/<ticket-id>/`
//! and emits two events. No cycle dirs are created.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cleanup_shorthand_deletes_ticket_dir() {
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

    // Pre-populate a stale ticket dir so we can prove the shorthand deleted it.
    let ticket_id = "ENG-99";
    let ticket_dir = session_root.join(ticket_id);
    std::fs::create_dir_all(&ticket_dir).unwrap();
    std::fs::write(ticket_dir.join("stale.txt"), "leftover").unwrap();
    assert!(
        ticket_dir.exists(),
        "pre-condition: ticket dir must exist before daemon runs"
    );

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
"#,
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
    // The shorthand `[[cleanup]]` has no `when.*` filter so it matches
    // unconditionally for any admitted ticket, regardless of status.
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

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s")
        .expect("child wait succeeds");
    assert!(status.success(), "binary should exit 0, got {status:?}");

    // Ticket dir must be gone.
    assert!(
        !ticket_dir.exists(),
        "ticket dir should be deleted by cleanup shorthand; remains at {ticket_dir:?}"
    );

    // Events file is a sibling of the (now-deleted) ticket dir.
    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let body = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|e| panic!("events.jsonl must exist at {events_path:?}: {e}"));
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "expected exactly 2 events; got:\n{body}");
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
    assert!(
        lines[0].contains("\"iters\":0"),
        "line 0 must have iters=0: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("\"event\":\"worktree_delete_requested\""),
        "line 1 must be worktree_delete_requested: {}",
        lines[1]
    );
    assert!(
        lines[1].contains("\"reason\":\"cleanup_shorthand\""),
        "line 1 must have reason=cleanup_shorthand: {}",
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
