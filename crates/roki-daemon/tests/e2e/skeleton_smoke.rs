//! End-to-end smoke test for the walking-skeleton daemon.
//!
//! Acceptance gate per design Testing Strategy "E2E / smoke": drives the
//! `roki` binary as a subprocess via `env!("CARGO_BIN_EXE_roki")` and
//! posts one Linear-shaped JSON body over loopback HTTP. Asserts:
//!
//! - Process exit code is zero (Req 8.2 — clean cycle returns 0 regardless
//!   of subprocess child exit code).
//! - The per-cycle stdout capture file contains `out` (Req 7.2).
//! - The per-cycle stderr capture file contains `err` (Req 7.2).
//! - A second POST issued before exit receives HTTP 503 (Req 8.4 —
//!   subsequent webhooks are rejected once the first admitted-and-matched
//!   cycle has begun execution).
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

    // 3. Build the workspace tempdir, WORKFLOW.toml, and roki.toml. The
    //    runner's `run.cmd` writes literal `out` to stdout and `err` to
    //    stderr so the assertions below can locate them in the per-cycle
    //    capture files.
    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).expect("create sessions dir");
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

    // 6. POST one Linear-shaped body. The runtime drains the channel and
    //    flips `cycle_started` to `true` before running the cycle, so the
    //    second POST below is racing against that flip.
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

    // 7. Issue a second POST. Three valid outcomes per Req 8.4 / handler
    //    contract:
    //
    //    - 503 from the handler (`cycle_started` already true, OR the
    //      receiver dropped after the cycle started, OR `try_send` returned
    //      `Full` because the runtime hadn't yet drained the first POST).
    //    - Connection refused (`Err`) when the daemon has already exited
    //      between the first response and this send — semantically a
    //      stronger guarantee than 503 (no listener at all).
    //
    //    A 202 here would violate the exactly-once cycle invariant.
    let resp2 = client.post(&webhook_url).json(&payload).send().await;
    match resp2 {
        Ok(r) => assert_eq!(
            r.status().as_u16(),
            503,
            "second POST must be rejected with 503 once cycle has started, got {}",
            r.status()
        ),
        Err(_) => {
            // Connection refused: the daemon already exited cleanly. This
            // is a stronger form of "rejected" than 503 — there is no
            // listener at all to accept a new cycle.
        }
    }

    // 8. Wait for the binary to exit. A 10s ceiling absorbs CI slowness
    //    while still failing fast if the daemon has wedged.
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("binary should exit within 10s")
        .expect("child wait succeeds");
    assert!(
        status.success(),
        "Req 8.2: binary should exit success on a clean cycle, got {status:?}"
    );

    // 9. Locate the per-cycle capture dir and read the stdout / stderr
    //    files. Layout per `capture::create`:
    //    `<session_root>/cycle-<uuid>/{stdout,stderr}` (skeleton scope per
    //    design Logical Data Model).
    let cycle_dir = std::fs::read_dir(&session_root)
        .expect("session_root readable")
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("cycle-")
        })
        .expect("cycle-<uuid> dir should exist under session_root");

    let stdout_bytes =
        std::fs::read_to_string(cycle_dir.path().join("stdout")).expect("read stdout file");
    let stderr_bytes =
        std::fs::read_to_string(cycle_dir.path().join("stderr")).expect("read stderr file");
    assert!(
        stdout_bytes.contains("out"),
        "Req 7.2: stdout capture must contain `out`, got {stdout_bytes:?}"
    );
    assert!(
        stderr_bytes.contains("err"),
        "Req 7.2: stderr capture must contain `err`, got {stderr_bytes:?}"
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
