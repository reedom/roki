//! Daemon refuses to start when `wt` or `ghq` is absent from PATH.
//! Confirms the structured event and the non-zero exit.

use std::process::Command;
use tempfile::TempDir;

#[cfg(unix)]
fn make_stub(dir: &std::path::Path, name: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh\nexit 0").unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&path, perm).unwrap();
}

fn write_minimal_config(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let session_root = tmp.join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
    let workflow_path = tmp.join("WORKFLOW.yaml");
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

    let roki_path = tmp.join("roki.toml");
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
    (roki_path, session_root)
}

#[test]
fn missing_wt_and_ghq_aborts_before_daemon_started() {
    let tmp = TempDir::new().unwrap();
    let (roki_path, session_root) = write_minimal_config(tmp.path());

    // Empty bin directory — neither wt nor ghq exists here.
    let empty_bin = tmp.path().join("empty-bin");
    std::fs::create_dir_all(&empty_bin).unwrap();

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

    // Exclusivity: the only event class allowed before the abort is
    // `daemon_dependency_missing`. A future regression that fires e.g.
    // `cold_start_began` before the dep gate would be caught here.
    for line in &lines {
        assert!(
            line.contains("\"event\":\"daemon_dependency_missing\""),
            "unexpected pre-abort event line: {line}\nfull body=\n{body}"
        );
    }
}

#[cfg(unix)]
#[test]
fn only_wt_missing_with_ghq_present_aborts_with_single_event() {
    let tmp = TempDir::new().unwrap();
    let (roki_path, session_root) = write_minimal_config(tmp.path());

    // PATH that contains a working `ghq` stub but no `wt`.
    let stub_bin = tmp.path().join("stub-bin");
    std::fs::create_dir_all(&stub_bin).unwrap();
    make_stub(&stub_bin, "ghq");

    let binary = env!("CARGO_BIN_EXE_roki");
    let out = Command::new(binary)
        .arg("run")
        .arg("--config")
        .arg(&roki_path)
        .env_clear()
        .env("PATH", &stub_bin)
        .env("HOME", tmp.path())
        .output()
        .expect("spawn roki");

    assert!(!out.status.success(), "daemon exited 0 with wt missing");

    let log = session_root.join("_daemon.events.jsonl");
    let body = std::fs::read_to_string(&log).unwrap();
    let dep_lines: Vec<&str> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"daemon_dependency_missing\""))
        .collect();
    assert_eq!(
        dep_lines.len(),
        1,
        "expected exactly one dep-missing event, got: {dep_lines:?}"
    );
    assert!(
        dep_lines[0].contains("\"binary\":\"wt\""),
        "expected wt as offender, got: {}",
        dep_lines[0]
    );
    assert!(
        !body.contains("\"event\":\"daemon_started\""),
        "daemon_started must not appear; body=\n{body}"
    );
}
