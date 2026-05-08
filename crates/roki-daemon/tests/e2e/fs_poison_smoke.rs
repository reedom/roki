//! E2E: session_root is a regular file, not a directory. The persistent
//! daemon (slice 5+) opens a daemon-scoped event log at startup; that open
//! fails with ENOTDIR before the webhook listener binds. The binary exits
//! non-zero with a startup error.
//!
//! Pre-slice-5 the daemon would have started, accepted the webhook, then
//! routed FsPoison through [[on_failure]] → recursion_bound and exited 1.
//! The post-slice-5 daemon fails earlier (startup) but still exits non-zero
//! when session_root is unusable, which is the contract this test pins.

use std::net::TcpListener;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn fs_poison_routes_through_on_failure() {
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

    let work = TempDir::new().expect("workspace tempdir");

    // Write a regular file at the path where session_root would be. The daemon
    // will try to open the events file at <session_root>/ENG-500.events.jsonl
    // and to create iter dirs at <session_root>/ENG-500/cycle-<uuid>/iter-1/.
    // Both fail because session_root is a file, not a directory.
    let session_root = work.path().join("sessions");
    std::fs::write(&session_root, b"not a dir").unwrap();
    assert!(
        session_root.is_file(),
        "pre-condition: sessions must be a file"
    );

    let ticket_id = "ENG-500";

    let workflow_path = work.path().join("WORKFLOW.toml");
    let workflow_body = format!(
        r#"
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
cmd = "true"

[[on_failure]]
when.kind = "fs_poison"
[on_failure.run]
cmd = "true"
"#
    );
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
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn roki binary");

    // Reference ticket_id and port to keep them used; the daemon exits at
    // startup before the listener binds, so no webhook is sent.
    let _ = (ticket_id, port);

    // The daemon's startup-time daemon-event-log open fails when
    // session_root is a regular file; the binary exits non-zero before the
    // webhook listener binds. Wait for the natural exit (this is one of the
    // few cases where the persistent daemon does NOT stay alive).
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("binary should exit within 10s on startup error")
        .expect("child wait succeeds");
    assert!(
        !status.success(),
        "binary should exit non-zero on fs_poison startup; got {status:?}"
    );
    // The events file cannot exist when session_root is a plain file;
    // no JSONL assertions are possible here.
}
