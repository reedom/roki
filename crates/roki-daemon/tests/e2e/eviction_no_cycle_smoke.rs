//! E2E (slice 6 Task 16): admission revoke after a cycle completes is
//! still cache-only — the worktree and session_tempdir on disk survive.
//!
//! Sequence:
//!   1. Webhook A (assignee=u1, status=todo) admits ticket; short rule
//!      cycle runs to `cycle_completed`.
//!   2. Webhook B (assignee=stranger) → admission rejects, dispatcher
//!      emits `webhook_skipped reason=assignee_mismatch` to the
//!      daemon-scoped log and marks the cache for eviction.
//!   3. Assert NO `worktree_delete_requested` and NO
//!      `session_tempdir_deleted` are emitted, and the on-disk
//!      `<session_root>/ENG-100/` and `<wt_root>/ENG-100/` directories
//!      still exist. Eviction is cache-only.

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
async fn admission_revoke_after_cycle_evicts_cache_only() {
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

    let ticket_id = "ENG-100";

    // Short rule with `pre` directing run so the worktree gets
    // materialized; the run phase is trivial so the cycle completes
    // quickly.
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
[rule.pre]
cmd = "printf '{\"directive\":\"run\"}'"
[rule.run]
cmd = "true"
[rule.post]
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"todo_done\"}'"
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
    let _ = await_daemon_ready(&session_root).await;

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let client = reqwest::Client::new();

    // Webhook A: admits + runs the rule cycle to completion.
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

    // Webhook B: assignee mismatch → admission rejects. Dispatcher emits
    // webhook_skipped and marks the cache entry for eviction. Worktree
    // and session_tempdir are RETAINED.
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

    // Brief grace period for any (incorrect) post-revoke disk side-effects to
    // surface in the event log before we assert their absence.
    sleep(Duration::from_millis(300)).await;

    sigterm_child(&child);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s after SIGTERM")
        .expect("child wait succeeds");
    assert!(
        status.success(),
        "binary should exit 0 after SIGTERM, got {status:?}"
    );

    // Per-ticket events: exactly 1 cycle_completed; no eviction-driven
    // worktree/tempdir delete events.
    let body = std::fs::read_to_string(&events_path).unwrap();
    let cycle_completed_count = body.matches("\"event\":\"cycle_completed\"").count();
    assert_eq!(
        cycle_completed_count, 1,
        "expected exactly 1 cycle_completed; got:\n{body}"
    );
    assert!(
        !body.contains("\"event\":\"worktree_delete_requested\""),
        "per-ticket log must not contain worktree_delete_requested:\n{body}"
    );
    assert!(
        !body.contains("\"event\":\"session_tempdir_deleted\""),
        "per-ticket log must not contain session_tempdir_deleted:\n{body}"
    );

    // Daemon-scoped log: webhook_skipped with assignee_mismatch reason.
    let daemon_body = std::fs::read_to_string(&daemon_events_path).unwrap();
    let skip_line = daemon_body
        .lines()
        .find(|l| l.contains("\"event\":\"webhook_skipped\""))
        .unwrap_or_else(|| panic!("expected webhook_skipped in daemon log:\n{daemon_body}"));
    assert!(
        skip_line.contains("\"reason\":\"assignee_mismatch\""),
        "webhook_skipped must carry reason=assignee_mismatch; got: {skip_line}"
    );
    assert!(
        skip_line.contains("\"ticket_id\":\"ENG-100\""),
        "webhook_skipped must reference ENG-100; got: {skip_line}"
    );
    assert!(
        !daemon_body.contains("\"event\":\"worktree_delete_requested\""),
        "daemon log must not contain worktree_delete_requested:\n{daemon_body}"
    );
    assert!(
        !daemon_body.contains("\"event\":\"session_tempdir_deleted\""),
        "daemon log must not contain session_tempdir_deleted:\n{daemon_body}"
    );

    // Disk: worktree + session_tempdir RETAINED (eviction is cache-only).
    let ticket_dir = session_root.join(ticket_id);
    assert!(
        ticket_dir.is_dir(),
        "session_tempdir must still exist at {ticket_dir:?} after admission revoke"
    );
    let wt_dir = wt_root.join(ticket_id);
    assert!(
        wt_dir.is_dir(),
        "worktree must still exist at {wt_dir:?} after admission revoke"
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
