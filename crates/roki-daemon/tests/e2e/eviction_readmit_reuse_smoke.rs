//! E2E (slice 6 Task 18): after admission revoke + re-admit, the
//! worktree on disk is reused (same inode) — proves the daemon does
//! NOT recreate the worktree across the eviction round-trip.
//!
//! Sequence:
//!   1. Webhook A (status=todo, assignee=u1) admits and runs a short
//!      cycle that materializes the worktree at `<wt_root>/ENG-100/`.
//!      Capture the inode.
//!   2. Webhook B (status=todo, assignee=stranger) is rejected →
//!      dispatcher emits `webhook_skipped reason=assignee_mismatch`
//!      and marks the cache for eviction.
//!   3. Webhook C (status=in_progress, assignee=u1) re-admits with a
//!      status diff so a SECOND cycle runs. Worktree must still be
//!      present at the same inode.
//!
//! Assertions:
//!   - Two `cycle_completed` events in the per-ticket log.
//!   - No `worktree_delete_requested` and no `session_tempdir_deleted`
//!     events (eviction is cache-only).
//!   - `<wt_root>/ENG-100` inode is identical before/after the revoke
//!     + re-admit dance — the worktree dir is reused, not recreated.

use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_ready, stub_empty_issues};

#[tokio::test]
async fn readmit_after_revoke_reuses_worktree_same_inode() {
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
    // Slice 8: cycle no longer materializes worktrees. Pre-seed the dir so
    // the inode-reuse and retention assertions have something to check.
    std::fs::create_dir_all(&wt_root).unwrap();

    let ticket_id = "ENG-100";
    std::fs::create_dir_all(wt_root.join(ticket_id)).unwrap();

    // Two short rules: status=todo (cycle 1, materializes worktree) and
    // status=in_progress (cycle 2, after revoke + re-admit).
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
      - id: pre0
        run: 'printf ''{\"directive\":\"run\"}'''
      - id: run0
        run: 'true'
      - id: post0
        run: 'printf ''{\"directive\":\"end\",\"outcome\":\"todo_done\"}'''
  - when:
      status: in_progress
    tasks:
      - id: pre1
        run: 'printf ''{\"directive\":\"run\"}'''
      - id: run1
        run: 'true'
      - id: post1
        run: 'printf ''{\"directive\":\"end\",\"outcome\":\"ip_done\"}'''
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

[default.ai]
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
    let _ = await_daemon_ready(&session_root).await;

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::new();

    // Webhook A: cycle 1 → worktree materialized.
    let payload_a = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": ticket_id,
            "assignee": {"id": "u1"},
            "state": {"name": "todo"},
            "labels": []
        }
    });
    let resp = client
        .post(&webhook_url)
        .json(&payload_a)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let daemon_events_path = session_root.join("_daemon.events.jsonl");

    wait_for_event_count(&events_path, "cycle_completed", 1, Duration::from_secs(15)).await;

    let wt_dir = wt_root.join(ticket_id);
    assert!(
        wt_dir.is_dir(),
        "cycle 1 must materialize worktree at {wt_dir:?}"
    );
    let inode_before = std::fs::metadata(&wt_dir)
        .expect("stat worktree before revoke")
        .ino();

    // Webhook B: assignee mismatch → admission rejects, cache marked
    // for eviction. Worktree on disk is RETAINED.
    let payload_b = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": ticket_id,
            "assignee": {"id": "stranger"},
            "state": {"name": "todo"},
            "labels": []
        }
    });
    let resp = client
        .post(&webhook_url)
        .json(&payload_b)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    wait_for_event_count(
        &daemon_events_path,
        "webhook_skipped",
        1,
        Duration::from_secs(10),
    )
    .await;

    // Brief pause so any (incorrect) on-disk reclamation would surface
    // before we observe.
    sleep(Duration::from_millis(300)).await;

    assert!(
        wt_dir.is_dir(),
        "worktree must remain on disk after admission revoke (cache-only eviction)"
    );

    // Webhook C: status diff → re-admit, cycle 2 fires reusing the
    // existing worktree (worktree::ensure fast-path: dir already exists).
    let payload_c = serde_json::json!({
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
        .json(&payload_c)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    wait_for_event_count(&events_path, "cycle_completed", 2, Duration::from_secs(15)).await;

    sigterm_child(&child);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s after SIGTERM")
        .expect("child wait succeeds");
    assert!(
        status.success(),
        "binary should exit 0 after SIGTERM, got {status:?}"
    );

    // Inode parity: same dir, never recreated.
    let inode_after = std::fs::metadata(&wt_dir)
        .expect("stat worktree after re-admit")
        .ino();
    assert_eq!(
        inode_before, inode_after,
        "worktree must be the SAME directory across the revoke + re-admit \
         dance (inode mismatch proves recreate). before={inode_before}, \
         after={inode_after}, path={wt_dir:?}"
    );

    // Cache-only invariant on the event logs.
    let body = std::fs::read_to_string(&events_path).unwrap();
    let cycle_completed_count = body.matches("\"event\":\"cycle_completed\"").count();
    assert_eq!(
        cycle_completed_count, 2,
        "expected exactly 2 cycle_completed events; got:\n{body}"
    );
    assert!(
        !body.contains("\"event\":\"worktree_delete_requested\""),
        "per-ticket log must not contain worktree_delete_requested:\n{body}"
    );
    assert!(
        !body.contains("\"event\":\"session_tempdir_deleted\""),
        "per-ticket log must not contain session_tempdir_deleted:\n{body}"
    );

    let daemon_body = std::fs::read_to_string(&daemon_events_path).unwrap();
    assert!(
        daemon_body
            .lines()
            .any(|l| l.contains("\"event\":\"webhook_skipped\"")
                && l.contains("\"reason\":\"assignee_mismatch\"")),
        "expected webhook_skipped reason=assignee_mismatch in daemon log:\n{daemon_body}"
    );
    assert!(
        !daemon_body.contains("\"event\":\"worktree_delete_requested\""),
        "daemon log must not contain worktree_delete_requested:\n{daemon_body}"
    );
    assert!(
        !daemon_body.contains("\"event\":\"session_tempdir_deleted\""),
        "daemon log must not contain session_tempdir_deleted:\n{daemon_body}"
    );

    // Session_tempdir also retained.
    let ticket_dir = session_root.join(ticket_id);
    assert!(
        ticket_dir.is_dir(),
        "session_tempdir must still exist at {ticket_dir:?}"
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
