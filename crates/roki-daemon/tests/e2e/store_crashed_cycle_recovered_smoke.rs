//! E2E (phase-3 store): a cycle interrupted by SIGKILL is closed out at
//! the next cold start with a `crashed_cycle_recovered` event and a
//! `failure` outcome stamped in SQLite.
//!
//! Flow:
//! 1. Boot the daemon with an admission whose `run` step sleeps long
//!    enough for SIGKILL to land mid-cycle. Wait for `cycle_started`.
//! 2. SIGKILL the daemon — no clean shutdown, so the cycle row stays at
//!    `ended_at IS NULL` in `roki.db`.
//! 3. Boot the daemon a second time. Assert:
//!    - `_daemon.events.jsonl` contains a `crashed_cycle_recovered` event.
//!    - The cycle row in SQLite now has `outcome = 'failure'` and
//!      `ended_at IS NOT NULL`.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_ready, await_daemon_ready_count, stub_empty_issues};

#[tokio::test]
async fn sigkill_mid_cycle_recovers_on_next_boot() {
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

    let ticket_id = "ENG-300";

    // 30s sleep guarantees the cycle stays in-flight when SIGKILL lands —
    // the test SIGKILLs after a brief delay so the runner has time to call
    // `Store::open_cycle` but no time to call `Store::close_cycle`.
    let workflow_path = work.path().join("WORKFLOW.yaml");
    let workflow_body = r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo

rules:
  - when:
      status: in_progress
    tasks:
      - id: long_running
        run: 'sh -c ''sleep 30'''
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

    // --- Boot #1: spawn a cycle and SIGKILL while it sleeps ---
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
        .expect("spawn roki binary #1");

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;
    let _ = await_daemon_ready(&session_root).await;

    let webhook_url = format!("http://127.0.0.1:{port}/");
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
    let resp = client.post(&webhook_url).json(&payload).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    // Wait until the cycle row appears in SQLite so we know the runner has
    // called `Store::open_cycle`. The cycle is in the run-phase sleep right
    // now; SIGKILL will leave the row with `ended_at IS NULL`.
    let store_path = session_root.join("roki.db");
    wait_for_inflight_row(&store_path, Duration::from_secs(15)).await;

    sigkill_child(&child);
    let _status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("binary #1 should exit promptly under SIGKILL")
        .expect("child #1 wait");

    // Confirm the row was left in-flight (sanity check; the cold-start
    // recovery in boot #2 is the actual assertion).
    {
        use roki_store::Store;
        let s = roki_store::SqliteStore::open(&store_path).expect("open store after boot #1");
        let inflight = s.list_inflight_cycles().expect("list inflight");
        assert!(
            !inflight.is_empty(),
            "boot #1 should have left at least one cycle in-flight"
        );
    }

    // --- Boot #2: cold start should close the orphan + emit recovery event ---
    let mut child2 = Command::new(binary)
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
        .expect("spawn roki binary #2");

    wait_for_listener(webhook_addr).await;
    await_daemon_ready_count(&session_root, 2).await;

    // Recovery event appears in the daemon-scoped event log.
    let daemon_events_path = session_root.join("_daemon.events.jsonl");
    wait_for_event_kind(
        &daemon_events_path,
        "crashed_cycle_recovered",
        Duration::from_secs(10),
    )
    .await;

    sigterm_child(&child2);
    let status2 = tokio::time::timeout(Duration::from_secs(15), child2.wait())
        .await
        .expect("binary #2 should exit within 15s after SIGTERM")
        .expect("child #2 wait");
    assert!(status2.success(), "binary #2 should exit 0, got {status2:?}");

    // SQLite assertion: the row is now closed with outcome=failure.
    {
        use roki_store::Store;
        let s = roki_store::SqliteStore::open(&store_path).expect("open store after boot #2");
        let inflight = s.list_inflight_cycles().expect("list inflight #2");
        assert!(
            inflight.is_empty(),
            "boot #2 should leave no in-flight cycles, got {inflight:?}"
        );
    }

    // Use the store API to read back the recovered cycle. boot #1 left
    // exactly one inflight row; after boot #2 closes it, the cycle id we
    // saw in step "wait_for_inflight_row" should now carry outcome=failure.
    let (recovered_cycle_id, _) = read_first_cycle_id(&store_path);
    let recovered = read_cycle_by_id(&store_path, &recovered_cycle_id)
        .expect("recovered cycle row visible via Store API");
    assert_eq!(
        recovered.outcome,
        Some(roki_store::models::CycleOutcome::Failure)
    );
    assert!(recovered.ended_at.is_some());
}

fn read_first_cycle_id(store_path: &std::path::Path) -> (String, Option<i64>) {
    // Pull the recovered cycle id from `_daemon.events.jsonl` — the
    // crashed_cycle_recovered event carries it explicitly. This avoids
    // requiring a list-all helper on the Store trait.
    let daemon_log = store_path
        .parent()
        .expect("session_root")
        .join("_daemon.events.jsonl");
    let body = std::fs::read_to_string(&daemon_log).expect("read daemon events");
    for line in body.lines() {
        if !line.contains("\"event\":\"crashed_cycle_recovered\"") {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).expect("parse event");
        let cid = v
            .get("cycle_id")
            .and_then(|s| s.as_str())
            .expect("cycle_id in event")
            .to_string();
        return (cid, None);
    }
    panic!("no crashed_cycle_recovered event found in {daemon_log:?}");
}

fn read_cycle_by_id(
    store_path: &std::path::Path,
    cycle_id: &str,
) -> Option<roki_store::models::Cycle> {
    use roki_store::Store;
    let s = roki_store::SqliteStore::open(store_path).expect("open store for get_cycle");
    s.get_cycle(cycle_id).expect("get_cycle ok")
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

async fn wait_for_event_kind(path: &std::path::Path, kind: &str, timeout: Duration) {
    let needle = format!("\"event\":\"{kind}\"");
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(body) = tokio::fs::read_to_string(path).await {
            if body.contains(&needle) {
                return;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {kind} in {}", path.display());
}

async fn wait_for_inflight_row(store_path: &std::path::Path, timeout: Duration) {
    use roki_store::Store;
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(s) = roki_store::SqliteStore::open(store_path) {
            if let Ok(rows) = s.list_inflight_cycles() {
                if !rows.is_empty() {
                    return;
                }
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for an in-flight cycle row in the store");
}

fn sigkill_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
}

fn sigterm_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}
