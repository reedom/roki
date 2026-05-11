//! Slice 9 e2e: `[api].port` set → `GET /api/healthz` returns the
//! `Healthz` payload (version, uptime, repos, api_request_count). Spec
//! fr:10 §`GET /api/healthz`.

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
async fn healthz_returns_runtime_metadata() {
    // Reserve two ephemeral ports up front — webhook + api — using the
    // bind-and-drop pattern. Loopback hands the OS just enough hysteresis
    // that the daemon can re-bind both before another process grabs them
    // in a parallel test run.
    let webhook_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let api_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let linear = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"viewer": {"id": "u1"}}
        })))
        .mount(&linear)
        .await;
    stub_empty_issues(&linear).await;

    let work = TempDir::new().unwrap();
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
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
        run: 'printf out'
"#;
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let roki_path = work.path().join("roki.toml");
    let roki_body = format!(
        r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = {webhook_port}

[default.ai]
cli = "echo"

[engine]

[paths]
workflow = "{workflow}"
session_root = "{session_root}"

[log]

[api]
bind = "127.0.0.1"
port = {api_port}
"#,
        webhook_port = webhook_port,
        api_port = api_port,
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

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], webhook_port).into();
    wait_for_listener(webhook_addr).await;
    let _ = await_daemon_ready(&session_root).await;

    // The API server is spawned in a tokio task that races with `daemon_ready`;
    // wait for the API port to accept connections before issuing the GET.
    let api_addr: SocketAddr = ([127, 0, 0, 1], api_port).into();
    wait_for_listener(api_addr).await;

    let resp: serde_json::Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{api_port}/api/healthz"))
        .send()
        .await
        .expect("healthz send")
        .json()
        .await
        .expect("healthz json");

    assert_eq!(resp["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        resp["api_request_count"].as_u64().unwrap() >= 1,
        "api_request_count must include this request: {resp}"
    );
    assert!(
        resp["configured_repositories"].as_array().is_some(),
        "configured_repositories must be an array: {resp}"
    );
    // Defensive: uptime is monotonic non-negative.
    assert!(resp["uptime_seconds"].as_u64().is_some());

    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0));
}

async fn wait_for_listener(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("listener never came up at {addr}");
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
