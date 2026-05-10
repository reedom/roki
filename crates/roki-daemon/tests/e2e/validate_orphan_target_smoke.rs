//! Slice 8 e2e: `roki workflow validate` rejects a YAML whose state machine
//! has an edge to an undeclared state. Validation accumulates every error
//! before exit and prints them all on stderr. Spec §12.1
//! "slice8-validate-orphan-target".

use std::process::Stdio;

use tempfile::TempDir;
use tokio::process::Command;

#[tokio::test]
async fn validate_reports_orphan_edge_target_and_exits_nonzero() {
    let work = TempDir::new().expect("workspace tempdir");
    let workflow_path = work.path().join("WORKFLOW.yaml");

    // Two errors:
    //   - rules[0].states.a.on_done -> "ghost" (state not declared, not a terminal)
    //   - rules[0].states.a.on_fail -> "phantom" (same)
    let workflow_body = r#"
admission:
  assignee: u1
  repos:
    - ghq: github.com/example/repo

rules:
  - when:
      status: in_progress
    start: a
    states:
      a:
        run: 'true'
        on_done: ghost
        on_fail: phantom
    terminals: {}
"#;
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let binary = env!("CARGO_BIN_EXE_roki");
    let output = Command::new(binary)
        .arg("workflow")
        .arg("validate")
        .arg(&workflow_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn validate");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "validate must exit non-zero on schema error; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("ghost"),
        "stderr must list orphan target 'ghost':\n{stderr}"
    );
    assert!(
        stderr.contains("phantom"),
        "stderr must list orphan target 'phantom' (multi-error accumulation):\n{stderr}"
    );
}
