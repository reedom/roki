//! Slice 8 e2e: `[[admission.repos]] workflow:` resolves a per-repo override
//! file relative to the top-level YAML; the override file's body parses,
//! sugar-expands, and validates clean. Daemon emits `daemon_ready`, proving
//! the override-loading path is wired (failure would surface as a startup
//! abort with a `per-repo override` error). Spec §12.1 "slice8-per-repo-override".
//!
//! Note: dispatcher consumption of `WorkflowConfig.repo_overrides` is a
//! follow-up; this fixture exercises load + parse + validate only, which is
//! the surface slice 8 actually delivers.

use std::net::TcpListener;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_ready, stub_empty_issues};

#[tokio::test]
async fn per_repo_override_file_loads_and_daemon_ready_fires() {
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
    stub_empty_issues(&linear).await;

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
    let wt_root = work.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();

    let workflow_path = work.path().join("WORKFLOW.yaml");
    let top_body = r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo
      workflow: repos/bar.yaml

rules:
  - when:
      status: in_progress
    tasks:
      - id: top
        run: 'true'
"#;
    std::fs::write(&workflow_path, top_body).unwrap();

    let repos_dir = work.path().join("repos");
    std::fs::create_dir_all(&repos_dir).unwrap();
    let bar_body = r#"
rules:
  - when:
      status: in_progress
    tasks:
      - id: from_bar
        run: 'true'
"#;
    std::fs::write(repos_dir.join("bar.yaml"), bar_body).unwrap();

    let roki_path = work.path().join("roki.toml");
    let roki_body = format!(
        r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = {port}

[default.ai]
cli = "echo"

[engine]

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

    let evt = await_daemon_ready(&session_root).await;
    assert_eq!(
        evt.get("event").and_then(|v| v.as_str()),
        Some("daemon_ready")
    );

    let exit = sigterm_and_wait(&mut child, Duration::from_secs(10)).await;
    assert_eq!(exit, Some(0));
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
