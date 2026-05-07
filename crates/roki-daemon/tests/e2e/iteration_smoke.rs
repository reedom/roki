//! End-to-end smoke for the slice 1 engine: drives a 2-iteration cycle
//! through the `roki` binary and asserts the per-iter layout.
//!
//! Pre returns `directive: "run"` in iter 1 and 2; post returns
//! `directive: "run"` in iter 1 (forcing a second iteration that skips pre)
//! and `directive: "end"` in iter 2. Run is a trivial printf that emits
//! known stdout / stderr.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cycle_loops_two_iterations_then_ends() {
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

    // The pre/post fake AI uses a tempfile counter so iter 1 and iter 2 emit
    // different directives without daemon-side state.
    let counter_path = work.path().join("counter");
    std::fs::write(&counter_path, "1").unwrap();

    // Pre: always emit `directive: "run"`.
    let pre_cmd = r#"printf '{"directive":"run","note":"pre-iter"}'"#;

    // Run: write known stdout/stderr.
    let run_cmd = r#"printf 'run-out'; printf 'run-err' 1>&2"#;

    // Post: read the counter; if 1, increment to 2 and emit `directive: "run"`;
    // if 2, emit `directive: "end"`.
    let post_cmd = format!(
        r#"
n=$(cat {counter})
if [ "$n" = "1" ]; then
    printf 2 > {counter}
    printf '{{"directive":"run","note":"post-iter-1"}}'
else
    printf '{{"directive":"end","note":"post-iter-2"}}'
fi
"#,
        counter = counter_path.display()
    );

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
[rule.pre]
cmd = {pre_cmd}
[rule.run]
cmd = {run_cmd}
[rule.post]
cmd = {post_cmd}
"#,
        pre_cmd = toml_string(pre_cmd),
        run_cmd = toml_string(run_cmd),
        post_cmd = toml_string(&post_cmd),
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

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let payload = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": "ENG-9",
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });
    let client = reqwest::Client::new();
    let resp = client.post(&webhook_url).json(&payload).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s")
        .expect("child wait succeeds");
    assert!(status.success(), "binary should exit success, got {status:?}");

    let ticket_dir = session_root.join("ENG-9");
    let cycle_entry = std::fs::read_dir(&ticket_dir)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .expect("cycle-<uuid> dir present");
    let cycle_path = cycle_entry.path();

    // Iter 1 must contain pre + run + post artefacts.
    let iter1 = cycle_path.join("iter-1");
    assert!(iter1.join("pre.stdout").is_file());
    assert!(iter1.join("pre.response.json").is_file());
    assert!(iter1.join("run.stdout").is_file());
    assert!(iter1.join("run.exit_code").is_file());
    assert!(iter1.join("post.stdout").is_file());
    assert!(iter1.join("post.response.json").is_file());

    let pre_resp = std::fs::read_to_string(iter1.join("pre.response.json")).unwrap();
    assert!(pre_resp.contains("\"directive\": \"run\""));
    let post_resp = std::fs::read_to_string(iter1.join("post.response.json")).unwrap();
    assert!(post_resp.contains("\"directive\": \"run\""));
    let run_out = std::fs::read_to_string(iter1.join("run.stdout")).unwrap();
    assert!(run_out.contains("run-out"));
    let run_exit = std::fs::read_to_string(iter1.join("run.exit_code")).unwrap();
    assert_eq!(run_exit.trim(), "0");

    // Iter 2 must skip pre (post=run skips pre on the next iteration), so
    // pre.stdout must NOT exist; run + post must.
    let iter2 = cycle_path.join("iter-2");
    assert!(iter2.is_dir(), "iter-2 dir must exist");
    assert!(!iter2.join("pre.stdout").exists(), "iter-2 must skip pre");
    assert!(iter2.join("run.stdout").is_file());
    assert!(iter2.join("run.exit_code").is_file());
    assert!(iter2.join("post.stdout").is_file());
    assert!(iter2.join("post.response.json").is_file());

    let post2_resp = std::fs::read_to_string(iter2.join("post.response.json")).unwrap();
    assert!(post2_resp.contains("\"directive\": \"end\""));
}

/// Minimal TOML quoter for embedding shell snippets in WORKFLOW.toml.
/// Escapes backslashes, double-quotes, and newlines so that multi-line
/// shell snippets survive inside a TOML basic string (`"..."`).
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
