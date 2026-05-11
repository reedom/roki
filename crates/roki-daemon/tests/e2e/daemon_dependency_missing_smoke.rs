//! fr:12 §"Missing dependency CLI": daemon refuses to start when `wt` or
//! `ghq` is absent from PATH. Confirms the structured event and the
//! non-zero exit.

use std::process::Command;
use tempfile::TempDir;

#[test]
fn missing_wt_and_ghq_aborts_before_daemon_started() {
    let tmp = TempDir::new().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    // Empty bin directory — neither wt nor ghq exists here.
    let empty_bin = tmp.path().join("empty-bin");
    std::fs::create_dir_all(&empty_bin).unwrap();

    // Minimal workflow and roki.toml so RokiConfig::load and
    // WorkflowConfig::load succeed. The dep check trips immediately after
    // those loads, so admission/rules content doesn't matter — only
    // the file structure does.
    let workflow_path = tmp.path().join("WORKFLOW.yaml");
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
        run: 'true'
"#;
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let roki_path = tmp.path().join("roki.toml");
    let roki_body = format!(
        r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = 12345

[default.ai]
cli = "echo"

[engine]
max_iterations = 1
shutdown_window_seconds = 1

[paths]
workflow = "{workflow}"
session_root = "{session_root}"

[log]
"#,
        workflow = workflow_path.display(),
        session_root = session_root.display(),
    );
    std::fs::write(&roki_path, roki_body).unwrap();

    let binary = env!("CARGO_BIN_EXE_roki");

    let out = Command::new(binary)
        .arg("run")
        .arg("--config")
        .arg(&roki_path)
        .env_clear()
        .env("PATH", &empty_bin)
        .env("HOME", tmp.path())
        .output()
        .expect("spawn roki");

    assert!(
        !out.status.success(),
        "daemon exited 0 with deps missing; stderr=\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let log = session_root.join("_daemon.events.jsonl");
    let body = std::fs::read_to_string(&log).unwrap_or_else(|e| {
        panic!(
            "daemon event log not present at {}: {e}\nstderr={}",
            log.display(),
            String::from_utf8_lossy(&out.stderr)
        )
    });

    let lines: Vec<&str> = body.lines().collect();

    let dep_lines: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.contains("\"event\":\"daemon_dependency_missing\""))
        .collect();
    assert!(
        dep_lines.iter().any(|l| l.contains("\"binary\":\"wt\"")),
        "missing wt dep line; body=\n{body}"
    );
    assert!(
        dep_lines.iter().any(|l| l.contains("\"binary\":\"ghq\"")),
        "missing ghq dep line; body=\n{body}"
    );

    assert!(
        !lines
            .iter()
            .any(|l| l.contains("\"event\":\"daemon_started\"")),
        "daemon_started must not appear when deps are missing; body=\n{body}"
    );
}
