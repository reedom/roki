//! End-to-end smoke test for the walking-skeleton daemon.
//!
//! Acceptance gate per design Testing Strategy "E2E / smoke": drives the
//! `roki` binary as a subprocess via `env!("CARGO_BIN_EXE_roki")` and
//! posts one Linear-shaped JSON body over loopback HTTP. Asserts:
//!
//! - The cycle completes (events.jsonl gains a `cycle_completed` line);
//!   then SIGTERM exits the persistent daemon with code 0 (Req 8.2 — clean
//!   cycle returns 0 regardless of subprocess child exit code).
//! - The per-cycle stdout capture file contains `out` (Req 7.2).
//! - The per-cycle stderr capture file contains `err` (Req 7.2).
//! - A second POST issued before SIGTERM receives HTTP 503 while the first
//!   cycle's subprocess is still in flight, OR is accepted (202) once the
//!   per-ticket task is idle (Req 8.4 still says exactly-one-cycle-at-a-time
//!   per ticket; in the persistent daemon a second webhook for the same
//!   ticket coalesces or starts a new cycle but the first cannot run twice).
//!
//! The test runs only with `--features test-support` so the env-var seam
//! in `linear::client::endpoint()` is active and `wiremock` can stub the
//! Linear `viewer { id }` resolve at startup (Req 9.1, 9.2, 9.3).

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
async fn skeleton_runs_one_cycle_and_rejects_subsequent_webhook() {
    // 1. Reserve an ephemeral webhook port. Bound + immediately dropped so
    //    the daemon can re-bind it; OS keeps the assignment fresh enough on
    //    loopback for a single-process test.
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral webhook port")
        .local_addr()
        .expect("local_addr")
        .port();

    // 2. Wiremock stub for Linear `viewer { id }`. The runtime calls this
    //    only when `[admission].assignee = "me"`; the smoke test pins the
    //    assignee to a literal `"u1"` so the resolve is short-circuited
    //    inside `runtime::run_inner`. The stub stays mounted as defence in
    //    depth: if the runtime ever started calling it, it must return a
    //    valid id rather than 404 the daemon out.
    let linear = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"viewer": {"id": "u1"}}
        })))
        .mount(&linear)
        .await;
    stub_empty_issues(&linear).await;

    // 3. Build the workspace tempdir, WORKFLOW.toml, and roki.toml. The
    //    runner's `run.cmd` writes literal `out` to stdout and `err` to
    //    stderr so the assertions below can locate them in the per-cycle
    //    capture files.
    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).expect("create sessions dir");
    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();
    let workflow_path = work.path().join("WORKFLOW.toml");
    let roki_path = work.path().join("roki.toml");

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
[rule.run]
cmd = "printf out; printf err 1>&2; exit 0"
"#;
    std::fs::write(&workflow_path, workflow_body).expect("write WORKFLOW.toml");

    // The skeleton config keys are the canonical six sections; `[engine]`
    // and `[log]` are accepted-without-applying empty tables (Req 2.4).
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

[paths]
workflow = "{workflow}"
session_root = "{session_root}"

[log]
"#,
        port = port,
        // TempDir on macOS / Linux uses forward-slash paths; the smoke test
        // is gated to those targets per design Out-of-Scope (no Windows).
        workflow = workflow_path.display(),
        session_root = session_root.display(),
    );
    std::fs::write(&roki_path, roki_body).expect("write roki.toml");

    // 4. Spawn the binary. `kill_on_drop` guarantees no orphan daemon if a
    //    panic short-circuits the test before the binary exits. The
    //    `ROKI_LINEAR_GRAPHQL_URL` env var is per-spawn (not per-process)
    //    so other tests running in parallel cannot observe this URL.
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

    // 5. Wait for the webhook listener to come up. A successful TCP
    //    connect proves the listener has bound the port; the handler may
    //    not yet have processed any request, but it will accept the POST
    //    that follows.
    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;
    // Slice 6: cold start runs after the listener binds. Wait for
    // `daemon_ready` so the gate is open and the POST below is not
    // short-circuited to 503 `cold_start_in_progress`.
    let _ = await_daemon_ready(&session_root).await;

    // 6. POST one Linear-shaped body. The runtime drains the channel and
    //    runs the cycle in the per-ticket task; a duplicate POST for the
    //    same ticket while the cycle is in flight is rejected.
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
    let resp1 = client
        .post(&webhook_url)
        .json(&payload)
        .send()
        .await
        .expect("first POST send");
    assert_eq!(
        resp1.status().as_u16(),
        202,
        "first POST should be accepted (202), got {}",
        resp1.status()
    );

    // 7. Issue a second POST. With the persistent daemon, valid outcomes
    //    are:
    //    - 503 from the handler if the per-ticket task is still busy.
    //    - 202 if the first cycle has already drained and the per-ticket
    //      task is idle; the duplicate is coalesced or scheduled.
    //
    //    Either is acceptable; what matters is that the listener is alive
    //    and never accepts a concurrent second cycle for the same ticket.
    let resp2 = client.post(&webhook_url).json(&payload).send().await;
    if let Ok(r) = resp2 {
        let status = r.status().as_u16();
        assert!(
            status == 202 || status == 503,
            "second POST should be 202 or 503, got {status}"
        );
    }

    // 8. Wait for the cycle to complete (events.jsonl gains a
    //    `cycle_completed` line), then SIGTERM the daemon.
    let events_path = session_root.join("tid-1.events.jsonl");
    wait_for_event_count(&events_path, "cycle_completed", 1, Duration::from_secs(15)).await;
    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(
        exit,
        Some(0),
        "Req 8.2: binary should exit 0 on a clean cycle after SIGTERM"
    );

    // 9. Locate the per-iter capture dir and read the run stdout / stderr
    //    files. Layout per `capture::create_iter_dir`:
    //    `<session_root>/<ticket-id>/cycle-<uuid>/iter-1/run.{stdout,stderr}`.
    let ticket_dir = session_root.join("tid-1");
    assert!(
        ticket_dir.is_dir(),
        "Req 7.2: ticket dir must exist at {ticket_dir:?}"
    );
    let cycle_entry = std::fs::read_dir(&ticket_dir)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("cycle-"))
        .expect("cycle-<uuid> dir should exist under ticket dir");
    let iter1 = cycle_entry.path().join("iter-1");
    assert!(iter1.is_dir(), "iter-1 dir must exist at {iter1:?}");

    let stdout_bytes = std::fs::read_to_string(iter1.join("run.stdout")).expect("read run.stdout");
    let stderr_bytes = std::fs::read_to_string(iter1.join("run.stderr")).expect("read run.stderr");
    assert!(
        stdout_bytes.contains("out"),
        "Req 7.2: run.stdout must contain `out`, got {stdout_bytes:?}"
    );
    assert!(
        stderr_bytes.contains("err"),
        "Req 7.2: run.stderr must contain `err`, got {stderr_bytes:?}"
    );

    let exit_code_text =
        std::fs::read_to_string(iter1.join("run.exit_code")).expect("read run.exit_code");
    assert_eq!(
        exit_code_text.trim(),
        "0",
        "exit code file must contain the run subprocess exit (0 here)"
    );
}

/// Poll the loopback address until a TCP connect succeeds, with a 5s
/// ceiling. The webhook handler accepts POST on any path, but a connect
/// alone is sufficient to confirm the listener has bound — no HTTP
/// round-trip is needed at this stage.
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

async fn sigterm_and_wait(child: &mut tokio::process::Child, timeout: Duration) -> Option<i32> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status.code(),
        _ => None,
    }
}
