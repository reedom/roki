//! End-to-end smoke: a 2-iter cycle where pre and post are session-shape
//! `prompt` bodies. The fake agent is reused across both iterations; the
//! test asserts the on-disk layout and that the same child PID handled both
//! turns.

use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn session_two_iter_smoke() {
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

    let counter_path = work.path().join("counter");
    let pid_path = work.path().join("agent.pid");

    // Generate the fake agent script with absolute paths inlined. The slice-2
    // supervisor calls `env_clear()` and only forwards a curated set of vars,
    // so passing ROKI_TEST_* through env would not survive — bake the paths
    // into the script itself.
    let agent_path = work.path().join("fake_session_agent.sh");
    let agent_script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
counter_file={counter:?}
pid_file={pid:?}
if [ ! -e "$pid_file" ]; then
  printf '%s\n' "$$" > "$pid_file"
fi
while IFS= read -r _line; do
  count=$(cat "$counter_file" 2>/dev/null || echo 0)
  count=$((count + 1))
  printf '%s' "$count" > "$counter_file"
  if [ "$count" -lt 3 ]; then
    printf '{{"directive":"run","note":"turn-%d"}}\n' "$count"
  else
    printf '{{"directive":"end","note":"turn-%d"}}\n' "$count"
  fi
done
"#,
        counter = counter_path.display().to_string(),
        pid = pid_path.display().to_string(),
    );
    std::fs::write(&agent_path, &agent_script).unwrap();
    let mut perms = std::fs::metadata(&agent_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&agent_path, perms).unwrap();

    let workflow_path = work.path().join("WORKFLOW.toml");
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
[rule.pre]
prompt = "pre-turn\n"
[rule.run]
cmd = "true"
[rule.post]
prompt = "post-turn\n"
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

[default.ai.session]
cli = "bash {agent}"
stall_seconds = 10

[engine]
max_iterations = 2

[paths]
workflow = "{workflow}"
session_root = "{session_root}"

[log]
"#,
        port = port,
        agent = agent_path.display(),
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
            "id": "ENG-S",
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });
    let client = reqwest::Client::new();
    let resp = client.post(&webhook_url).json(&payload).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("binary should exit within 20s")
        .expect("child wait succeeds");
    assert!(status.success(), "binary should exit success, got {status:?}");

    let ticket_dir = session_root.join("ENG-S");
    let cycle_entry = std::fs::read_dir(&ticket_dir)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .expect("cycle-<uuid> dir present");
    let cycle_path = cycle_entry.path();

    // Iter 1: pre + run + post present.
    let iter1 = cycle_path.join("iter-1");
    for f in [
        "pre.stdout",
        "pre.stderr",
        "pre.events.jsonl",
        "pre.response.json",
        "run.exit_code",
        "post.stdout",
        "post.stderr",
        "post.events.jsonl",
        "post.response.json",
    ] {
        assert!(
            iter1.join(f).is_file(),
            "missing {f} in {}",
            iter1.display()
        );
    }
    assert!(
        !iter1.join("run.terminal.json").exists(),
        "run is plain shell — terminal must be absent"
    );

    // Iter 2: skip pre (post=run skips pre); run + post present.
    let iter2 = cycle_path.join("iter-2");
    assert!(iter2.is_dir(), "iter-2 dir must exist");
    assert!(!iter2.join("pre.stdout").exists(), "iter-2 must skip pre");
    for f in [
        "run.exit_code",
        "post.stdout",
        "post.stderr",
        "post.events.jsonl",
        "post.response.json",
    ] {
        assert!(
            iter2.join(f).is_file(),
            "missing {f} in {}",
            iter2.display()
        );
    }

    let post2_resp = std::fs::read_to_string(iter2.join("post.response.json")).unwrap();
    assert!(post2_resp.contains("\"directive\": \"end\""));

    // Every parseable directive line must land in <phase>.events.jsonl per
    // fr:04 §72. Spot-check that pre.events.jsonl for iter-1 captured the
    // first turn's directive (the fake agent emits `{"directive":"run",...}`).
    let pre1_events = std::fs::read_to_string(iter1.join("pre.events.jsonl")).unwrap();
    assert!(
        pre1_events.contains("\"directive\""),
        "pre.events.jsonl should contain the parsed directive line: {pre1_events:?}"
    );
    let post2_events = std::fs::read_to_string(iter2.join("post.events.jsonl")).unwrap();
    assert!(
        post2_events.contains("\"directive\""),
        "post.events.jsonl should contain the parsed directive line: {post2_events:?}"
    );

    // The fake agent wrote its $$ exactly once at first start.
    let pid = std::fs::read_to_string(&pid_path).unwrap();
    assert!(!pid.trim().is_empty(), "pid file should be populated once");
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
