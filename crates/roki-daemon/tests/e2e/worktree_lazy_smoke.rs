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

    let mut child = spawn_one();
    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;
    post_webhook(port, ticket_id).await;

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "cycle 1 must exit 0");
    assert!(
        wt_root.join(ticket_id).is_dir(),
        "worktree must be created in cycle 1"
    );

    // ---------- Cycle 2: same ticket; worktree must be reused (still on disk) ----------
    let mut child = spawn_one();
    wait_for_listener(webhook_addr).await;
    post_webhook(port, ticket_id).await;
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "cycle 2 must exit 0");
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
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "cycle 3 must exit 0");
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
        .post(&format!("http://127.0.0.1:{port}/"))
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
