//! E2E (slice 6): cold-start partial enumeration skips orphan reconcile.
//!
//! Sequence:
//!   1. wiremock returns page 1 (1 ticket + hasNextPage:true,
//!      endCursor:"c1") on the first issues query, then 500 on page 2.
//!   2. Cold start surfaces `enum_partial: true` with
//!      `partial_reason: "linear_unreachable"`.
//!   3. Per fr:07 §4.6, partial enum SKIPS orphan reconcile. The
//!      pre-seeded `<session_root>/old-orphan-1` therefore survives the
//!      daemon launch unchanged, and an `orphan_reconcile_skipped`
//!      event is emitted to `_daemon.events.jsonl`.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_cold_start_completed, await_daemon_event, issue_node};

#[tokio::test]
async fn cold_start_partial_enum_skips_orphan_reconcile() {
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

    // Page 1: one ticket, hasNextPage:true, endCursor:"c1". Bound to one
    // hit so the second issues query falls through to the 500 stub.
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                    "nodes": [
                        issue_node("p1", "TEAM-1", "todo", "u1"),
                    ]
                }
            }
        })))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&linear)
        .await;

    // Subsequent issues queries 500. Same priority as page-1 mock; once
    // page 1's quota is consumed, this becomes the active issues
    // responder. `LinearGraphqlClient::enumerate` maps non-success HTTP
    // status to `LinearEnumerateError::NonSuccess`, which classifies as
    // `partial_reason: "linear_unreachable"`.
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(500).set_body_string(""))
        .with_priority(1)
        .mount(&linear)
        .await;

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    // Pre-seed an orphan dir. Partial enum must NOT delete it.
    std::fs::create_dir_all(session_root.join("old-orphan-1")).unwrap();

    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    let workflow_path = work.path().join("WORKFLOW.yaml");
    let workflow_body = r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo

rules:
  - when:
      status: todo
    tasks:
      - id: run0
        run: 'true'
      - id: post0
        run: 'printf ''{\"directive\":\"end\",\"outcome\":\"done\"}'''
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

[default]
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
        // Force one ticket per page so the second page is reached.
        .env("ROKI_COLD_START_PAGE_SIZE", "1")
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn roki binary");

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;

    let report = await_cold_start_completed(&session_root).await;
    assert_eq!(report["enum_partial"], true, "report = {report}");
    assert_eq!(
        report["partial_reason"], "linear_unreachable",
        "report = {report}"
    );
    // Orphan reconcile is skipped on partial enum.
    assert_eq!(report["orphans_deleted"], 0, "report = {report}");

    // The skip event must accompany cold_start_completed (cold_start.rs
    // emits OrphanReconcileSkipped before ColdStartCompleted on the
    // partial path, so by the time the report lands, the skip event is
    // already on disk).
    let skip = await_daemon_event(
        &session_root,
        "orphan_reconcile_skipped",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(skip["reason"], "cold_start_partial", "skip = {skip}");

    // The pre-seeded orphan must still exist.
    assert!(
        session_root.join("old-orphan-1").exists(),
        "old-orphan-1 must survive partial-enum cold start"
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
