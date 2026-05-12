//! E2E (phase-2 store): the SQLite store is the authoritative admission
//! audit log, and survives daemon restarts.
//!
//! Flow:
//! 1. Boot the daemon. Empty Linear `issues` cold-start stub. Post one
//!    webhook to admit `tid-1`. Wait for `cycle_completed`. SIGTERM.
//! 2. Open the SQLite store (`<session_root>/roki.db`) directly via
//!    `roki_store::SqliteStore` and assert:
//!    - `list_admitted()` contains `tid-1` with the configured repo path.
//!    - The triple-write of the admission survived process exit.
//! 3. Boot the daemon a second time with the same `roki.toml` (same
//!    `session_root`, same DB). Empty Linear `issues` stub again. Wait for
//!    `daemon_ready`. SIGTERM.
//! 4. Open the SQLite store again and assert the second boot's cold-start
//!    hint did NOT spuriously double-evict / double-flap the row: the
//!    same admission row is still present (Linear truth says "admit it"
//!    in cold-start enumeration if the empty stub returned anything; with
//!    an empty stub, the ticket should remain admitted as the GraphQL
//!    pass found nothing to reconcile — and the row is the operator's
//!    audit log).
//!
//! Smoke target: this proves admission state is mirrored to SQLite on
//! webhook admit and persists across daemon restart.

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
async fn store_persists_admission_across_daemon_restart() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral webhook port")
        .local_addr()
        .expect("local_addr")
        .port();
    let api_port = TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral api port")
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
    std::fs::create_dir_all(&session_root).expect("create sessions dir");
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
      status: in_progress
    tasks:
      - id: run0
        run: 'true'
"#;
    std::fs::write(&workflow_path, workflow_body).expect("write WORKFLOW.yaml");

    let roki_path = work.path().join("roki.toml");
    let roki_body = format!(
        r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = {port}

[api]
bind = "127.0.0.1"
port = {api_port}

[default.ai]
cli = "echo"

[engine]

[paths]
workflow = "{workflow}"
session_root = "{session_root}"

[log]
"#,
        port = port,
        api_port = api_port,
        workflow = workflow_path.display(),
        session_root = session_root.display(),
    );
    std::fs::write(&roki_path, roki_body).expect("write roki.toml");

    let binary = env!("CARGO_BIN_EXE_roki");

    // --- Boot #1: admit one ticket via webhook ---
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
            "id": "tid-1",
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });
    let client = reqwest::Client::new();
    let resp = client.post(&webhook_url).json(&payload).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 202, "first POST should be 202");

    let events_path = session_root.join("tid-1.events.jsonl");
    wait_for_event_count(&events_path, "cycle_completed", 1, Duration::from_secs(15)).await;

    sigterm_child(&child);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary #1 should exit within 15s after SIGTERM")
        .expect("child #1 wait succeeds");
    assert!(status.success(), "binary #1 should exit 0, got {status:?}");

    // --- Inspect SQLite store between boots ---
    let store_path = session_root.join("roki.db");
    assert!(
        store_path.exists(),
        "roki.db should exist at {store_path:?}"
    );
    {
        let store =
            roki_store::SqliteStore::open(&store_path).expect("open store after boot #1");
        use roki_store::Store;
        let admitted = store.list_admitted().expect("list_admitted after boot #1");
        assert_eq!(
            admitted.len(),
            1,
            "store should hold exactly one admitted ticket after boot #1, got {admitted:?}"
        );
        assert_eq!(admitted[0].id, "tid-1");
        assert_eq!(admitted[0].repo, "github.com/example/repo");
        assert!(
            admitted[0].evicted_at.is_none(),
            "ticket should not be evicted after boot #1"
        );
    }

    // --- Boot #2: no Linear traffic; verify the row survives ---
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
    // The events file is opened in append mode, so the second daemon_ready
    // is the second occurrence on disk.
    await_daemon_ready_count(&session_root, 2).await;

    // Prove the cold-start hint pre-seeded the in-memory cache: no webhook
    // has fired in boot #2 and the GraphQL enumerate returned no issues, so
    // the only way `tid-1` can appear in `GET /api/tickets` is via the
    // `seed_from_store_hint` path that consults `Store::list_admitted`.
    let api_addr: SocketAddr = ([127, 0, 0, 1], api_port).into();
    wait_for_listener(api_addr).await;
    let tickets_url = format!("http://127.0.0.1:{api_port}/api/tickets");
    let body = reqwest::get(&tickets_url)
        .await
        .expect("GET /api/tickets")
        .text()
        .await
        .expect("read api body");
    assert!(
        body.contains("tid-1"),
        "boot #2 /api/tickets should list the seeded ticket, got: {body}"
    );

    sigterm_child(&child2);
    let status2 = tokio::time::timeout(Duration::from_secs(15), child2.wait())
        .await
        .expect("binary #2 should exit within 15s after SIGTERM")
        .expect("child #2 wait succeeds");
    assert!(status2.success(), "binary #2 should exit 0, got {status2:?}");

    // After boot #2, the row must still exist. The empty Linear stub means
    // cold-start did not re-admit (and did not evict via orphan reconcile
    // because cold-start orphan reconcile only touches session tempdirs,
    // not store rows). The phase-2 contract is that admission lifecycle is
    // mirrored to the store; nothing in boot #2 should have flipped it.
    {
        let store =
            roki_store::SqliteStore::open(&store_path).expect("open store after boot #2");
        use roki_store::Store;
        let admitted = store.list_admitted().expect("list_admitted after boot #2");
        assert_eq!(
            admitted.len(),
            1,
            "store should still hold the admitted ticket after boot #2 (cold-start hint), got {admitted:?}"
        );
        assert_eq!(admitted[0].id, "tid-1");
    }
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

fn sigterm_child(child: &tokio::process::Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}
