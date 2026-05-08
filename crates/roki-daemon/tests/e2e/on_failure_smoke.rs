//! E2E: rule's post phase exits non-zero (process_crash). A matching
//! [[on_failure]] handler runs and emits {"directive":"end"}, so the daemon
//! exits 0. Events file has exactly one cycle_completed event with
//! cycle_kind=failure and zero failure_unhandled events. Both the failed rule
//! cycle's iter dir and the handler cycle's iter dir exist on disk.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn on_failure_handler_recovers_from_process_crash() {
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

    let ticket_id = "ENG-200";

    let workflow_path = work.path().join("WORKFLOW.toml");
    // The post phase exits non-zero with no JSON output, which the directive
    // parser classifies as ProcessCrash. The on_failure handler matches
    // when.kind = "process_crash" and its post emits {"directive":"end"}.
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
[rule.post]
cmd = "exit 7"

[[on_failure]]
when.kind = "process_crash"
[on_failure.run]
cmd = "echo handled"
[on_failure.post]
cmd = "printf '{{\"directive\":\"end\",\"outcome\":\"handled\"}}'"
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
            "id": ticket_id,
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
        "binary should exit 0 when handler succeeds; got {status:?}"
    );

    // Events file: exactly one cycle_completed with cycle_kind=failure;
    // zero failure_unhandled events.
    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let body = std::fs::read_to_string(&events_path)
        .unwrap_or_else(|e| panic!("events.jsonl must exist at {events_path:?}: {e}"));
    let lines: Vec<&str> = body.lines().collect();

    assert_eq!(
        lines.len(),
        1,
        "expected exactly 1 event (cycle_completed for the handler); got:\n{body}"
    );
    assert!(
        lines[0].contains("\"event\":\"cycle_completed\""),
        "line 0 must be cycle_completed: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("\"cycle_kind\":\"failure\""),
        "line 0 must have cycle_kind=failure: {}",
        lines[0]
    );
    assert!(
        !body.contains("\"event\":\"failure_unhandled\""),
        "no failure_unhandled events expected; got:\n{body}"
    );

    // Both the failed rule cycle's iter dir and the handler cycle's iter dir
    // must exist. Look for two distinct cycle-<uuid> dirs under the ticket dir.
    let ticket_dir = session_root.join(ticket_id);
    assert!(
        ticket_dir.exists(),
        "ticket dir must exist at {ticket_dir:?}"
    );

    let cycle_dirs: Vec<_> = std::fs::read_dir(&ticket_dir)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .collect();

    assert_eq!(
        cycle_dirs.len(),
        2,
        "expected 2 cycle dirs (failed rule + handler); found {}; dirs: {:?}",
        cycle_dirs.len(),
        cycle_dirs.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );

    for entry in &cycle_dirs {
        let iter_dir = entry.path().join("iter-1");
        assert!(
            iter_dir.exists(),
            "iter-1 must exist under {:?}",
            entry.path()
        );
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
