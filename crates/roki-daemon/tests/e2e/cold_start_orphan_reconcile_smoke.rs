//! E2E (slice 6): cold-start orphan reconcile deletes pre-existing
//! session_root/<id> directories whose ids are NOT in the Linear-API
//! enumerate hit set. The newly-enumerated ticket's per-ticket session
//! tempdir survives.
//!
//! Sequence:
//!   1. Pre-create `<session_root>/old-orphan-1` and
//!      `<session_root>/old-orphan-2` before launching the daemon.
//!   2. wiremock returns ONE ticket with id `new-1` from the GraphQL
//!      `issues` query.
//!   3. After `cold_start_completed`, assert:
//!        - report["orphans_deleted"] == 2
//!        - the two old dirs are gone
//!          and after `daemon_ready`, the new ticket's session_tempdir
//!          either exists (cycle has begun and admission opened the dir)
//!          or its events file exists. Either is acceptable evidence of
//!          admission; the orphan-reconcile contract per fr:07 §Cold start
//!          step 5 is the deletion of the two old dirs.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_cold_start_completed, await_daemon_ready, issue_node};

#[tokio::test]
async fn cold_start_deletes_orphan_session_tempdirs() {
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

    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [
                        issue_node("new-1", "TEAM-1", "todo", "u1"),
                    ]
                }
            }
        })))
        .with_priority(1)
        .mount(&linear)
        .await;

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    // Pre-seed two orphan session-tempdirs before the daemon launches.
    // Cold-start orphan reconcile must delete both.
    std::fs::create_dir_all(session_root.join("old-orphan-1")).unwrap();
    std::fs::create_dir_all(session_root.join("old-orphan-2")).unwrap();

    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    let workflow_path = work.path().join("WORKFLOW.toml");
    let workflow_body = r#"
[admission]
assignee = "u1"

[[admission.repos]]
ghq = "github.com/example/repo"

[[rule]]
[rule.when]
status = "todo"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "true"
[rule.post]
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"done\"}'"
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

    let report = await_cold_start_completed(&session_root).await;
    assert_eq!(report["enumerated"], 1, "report = {report}");
    assert_eq!(report["admitted"], 1, "report = {report}");
    assert_eq!(report["cycles_spawned"], 1, "report = {report}");
    assert_eq!(report["orphans_deleted"], 2, "report = {report}");
    assert_eq!(report["enum_partial"], false, "report = {report}");

    // The two pre-seeded orphans are gone.
    assert!(
        !session_root.join("old-orphan-1").exists(),
        "old-orphan-1 should have been deleted by orphan reconcile"
    );
    assert!(
        !session_root.join("old-orphan-2").exists(),
        "old-orphan-2 should have been deleted by orphan reconcile"
    );

    let _ = await_daemon_ready(&session_root).await;

    // The new ticket should have produced its events file (cycle dispatch
    // opens it via real_runner before any worktree work). The session
    // tempdir itself is created lazily by the cycle's session-shape
    // executor, so we accept either the dir OR the events sibling file
    // as evidence of admission. The events file is the more reliable
    // signal because slice 1 always opens it before any phase runs.
    let new_events = session_root.join("new-1.events.jsonl");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        if new_events.exists() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(
        new_events.exists(),
        "expected new-1.events.jsonl to be created by the cold-start cycle"
    );

    // Clean shutdown.
    sigterm_child(&child);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s after SIGTERM")
        .expect("child wait succeeds");
    assert!(
        status.success(),
        "binary should exit 0 after SIGTERM, got {status:?}"
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

fn sigterm_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}
