//! E2E (slice 6): cold start under `roki cleanup` (CleanupOnly dispatch
//! mode) only runs cycles that match a `[[cleanup]]` entry. Tickets that
//! would match a `[[rule]]` under default mode are admitted to the cache
//! but never produce a cycle, because `evaluate(.., CleanupOnly)` skips
//! the rule list.
//!
//! Setup:
//!   - Workflow has BOTH a `[[rule]] when.status="todo"` and a
//!     `[[cleanup]] when.status="done"`.
//!   - Cold-start enumerate returns TWO tickets:
//!       - `todo-1`  status=todo  -> rule-only match (ignored in CleanupOnly)
//!       - `done-1`  status=done  -> cleanup match    (runs)
//!   - Daemon launched with the `cleanup` subcommand.
//!
//! Assertions:
//!   - `cold_start_completed` reports enumerated=2, admitted=2.
//!     `cycles_spawned` reflects the dispatcher's `admit_for_cold_start`
//!     count, not the post-evaluate match count, so the load-bearing
//!     assertion is the per-ticket evidence below.
//!   - `done-1.events.jsonl` exists and contains `cycle_completed` with
//!     `cycle_kind=cleanup`. The cleanup directive deletes the ticket
//!     dir, so we also assert `worktree_delete_requested` is emitted.
//!   - `todo-1.events.jsonl` either does not exist OR contains no
//!     `cycle_started` line — the rule list is ignored in CleanupOnly
//!     mode and the task short-circuits with `StepOutcome::NoMatch`
//!     before any cycle event lands on disk.

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
async fn cold_start_cleanup_mode_dispatches_only_cleanup_match() {
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

    // Two enumerated tickets: one rule-shaped (todo), one cleanup-shaped (done).
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [
                        issue_node("todo-1", "TEAM-1", "todo", "u1"),
                        issue_node("done-1", "TEAM-2", "done", "u1"),
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
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"todo_done\"}'"

[[cleanup]]
[cleanup.when]
status = "done"
[cleanup.when.labels]
has_all = []
[cleanup.run]
cmd = "echo cleanup-run"
[cleanup.post]
cmd = "printf '{\"directive\":\"end\",\"outcome\":\"cleanup_done\"}'"
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
    // `roki cleanup` => DispatchMode::CleanupOnly, propagated through
    // cold_start.rs and into the per-ticket task evaluator.
    let mut child = Command::new(binary)
        .arg("cleanup")
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

    // cold_start_completed lands first. Both tickets pass admission
    // (assignee + repo), so admitted == 2 regardless of dispatch mode.
    // `cycles_spawned` mirrors `Dispatcher::admit_for_cold_start` calls —
    // not the post-evaluate match count — so we don't pin it. The
    // load-bearing assertions are the per-ticket evidence below.
    let report = await_cold_start_completed(&session_root).await;
    assert_eq!(report["enumerated"], 2, "report = {report}");
    assert_eq!(report["admitted"], 2, "report = {report}");
    assert_eq!(report["enum_partial"], false, "report = {report}");

    let _ = await_daemon_ready(&session_root).await;

    // The cleanup-matching ticket runs a cleanup cycle and the terminal
    // directive deletes its session tempdir. Wait for the
    // `worktree_delete_requested` event — cleanup_cycle_smoke uses the
    // same signal as the proof of "cleanup cycle ran to completion".
    let done_events = session_root.join("done-1.events.jsonl");
    wait_for_event_count(
        &done_events,
        "worktree_delete_requested",
        1,
        Duration::from_secs(15),
    )
    .await;

    let done_body = std::fs::read_to_string(&done_events)
        .unwrap_or_else(|e| panic!("done-1 events.jsonl must exist at {done_events:?}: {e}"));
    // Slice 1 only emits `cycle_completed` per cycle (no separate
    // `cycle_started`), so that is the proof-of-cycle signal here.
    assert!(
        done_body.contains("\"event\":\"cycle_completed\""),
        "done-1 must have cycle_completed:\n{done_body}"
    );
    assert!(
        done_body.contains("\"cycle_kind\":\"cleanup\""),
        "done-1 cycle must be cycle_kind=cleanup:\n{done_body}"
    );
    assert!(
        done_body.contains("\"reason\":\"cleanup_terminal\""),
        "done-1 must have worktree_delete_requested with reason=cleanup_terminal:\n{done_body}"
    );

    // Give the rule-only ticket a brief window to (mistakenly) emit a
    // cycle_started before we assert it didn't. NoMatch short-circuits
    // before the writer touches the per-ticket events file, so 500ms is
    // ample.
    sleep(Duration::from_millis(500)).await;
    let todo_events = session_root.join("todo-1.events.jsonl");
    if todo_events.exists() {
        let body = std::fs::read_to_string(&todo_events).unwrap();
        assert!(
            !body.contains("\"event\":\"cycle_completed\""),
            "todo-1 must not have cycle_completed in CleanupOnly mode; got:\n{body}"
        );
    }

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
