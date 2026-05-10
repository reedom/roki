//! Slice 8 e2e: `roki workflow graph` renders ASCII for a sugar `tasks:`
//! rule. Verifies the binary parses + sugar-expands + validates + emits a
//! human-readable state graph snapshot. Spec §12.1
//! "slice8-workflow-graph-cli".

use std::process::Stdio;

use tempfile::TempDir;
use tokio::process::Command;

#[tokio::test]
async fn workflow_graph_renders_ascii_for_sugar_chain() {
    let work = TempDir::new().expect("workspace tempdir");
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
      - id: a
        run: 'true'
      - id: b
        run: 'true'
"#;
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let binary = env!("CARGO_BIN_EXE_roki");
    let output = Command::new(binary)
        .arg("workflow")
        .arg("graph")
        .arg(&workflow_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn workflow graph");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "graph must succeed; stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("rules[0]"),
        "ASCII output must label the rule selector; got:\n{stdout}"
    );
    assert!(
        stdout.contains('a') && stdout.contains('b'),
        "ASCII output must list both state ids; got:\n{stdout}"
    );
    assert!(
        stdout.contains("__success__"),
        "ASCII output must show the auto-injected terminal; got:\n{stdout}"
    );
}
