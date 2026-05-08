//! End-to-end smoke: a run phase emits a claude/codex stream-json `result`
//! event on stdout. The supervisor's tee scanner extracts it into
//! `iter-1/run.terminal.json`. The post phase reads
//! `{{ run.terminal.is_error }}` from the Liquid context and emits its value
//! on stderr; the test asserts both the on-disk file and the post stderr.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn run_terminal_round_trips_through_liquid() {
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
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    // Run phase: emit a thinking event then a result event with is_error=false.
    let run_cmd = r#"printf '%s\n' '{"type":"thinking","text":"working"}' '{"type":"result","subtype":"success","is_error":false,"result":"ok"}'"#;

    // Post phase: emit `terminal_is_error=<value>` on stderr (verifies Liquid
    // round-trip), then write the directive on stdout.
    let post_cmd = r#"printf 'terminal_is_error=%s\n' "{{ run.terminal.is_error }}" 1>&2; printf '{"directive":"end"}'"#;

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
cmd = {run_quoted}
[rule.post]
cmd = {post_quoted}
"#,
        run_quoted = toml_string(run_cmd),
        post_quoted = toml_string(post_cmd),
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
cli = "true"
stall_seconds = 30

[engine]
max_iterations = 1

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

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let payload = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": "ENG-RT",
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(&webhook_url)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s")
        .expect("child wait succeeds");
    assert!(
        status.success(),
        "binary should exit success, got {status:?}"
    );

    let cycle_root = session_root.join("ENG-RT");
    let cycle_entry = std::fs::read_dir(&cycle_root)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .expect("cycle-<uuid> dir present");
    let cycle_path = cycle_entry.path();
    let iter_dir = cycle_path.join("iter-1");

    // run.terminal.json exists and contains is_error.
    let run_terminal = std::fs::read_to_string(iter_dir.join("run.terminal.json"))
        .expect("run.terminal.json must exist");
    assert!(
        run_terminal.contains("\"is_error\""),
        "run.terminal.json missing is_error: {run_terminal}"
    );
    assert!(
        run_terminal.contains("\"subtype\""),
        "run.terminal.json missing subtype: {run_terminal}"
    );

    // Post stderr proves the Liquid round-trip rendered the value.
    let post_stderr =
        std::fs::read_to_string(iter_dir.join("post.stderr")).expect("post.stderr must exist");
    assert!(
        post_stderr.contains("terminal_is_error=false"),
        "post.stderr should contain rendered is_error=false: {post_stderr}"
    );
}

fn toml_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
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
