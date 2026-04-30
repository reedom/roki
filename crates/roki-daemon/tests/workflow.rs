//! Integration tests for the `WORKFLOW.md` loader (task 2.3).
//!
//! These tests exercise the file-on-disk + filesystem-watch paths that the
//! inline unit tests in `src/workflow/mod.rs` deliberately do not cover.

use std::time::Duration;

use roki_daemon::workflow::{WorkflowLoader, WorkflowPolicy};
use tempfile::TempDir;
use tokio::time::timeout;

/// Construct a valid `WORKFLOW.md` body that exercises every reserved
/// extension namespace.
fn valid_workflow_with_all_namespaces() -> &'static str {
    r#"---
sandbox: workspace-write
elicitations: reject
max_turns: 25
stall_window_seconds: 90
backoff:
  min_seconds: 12
  max_seconds: 240
extension:
  gates:
    spec:
      required_phases: ["requirements", "design", "tasks"]
      block_on_skipped: true
    review:
      block_on_findings: true
      reviewer_quorum: 2
  server:
    bind: "127.0.0.1:7777"
    static_root: "./public"
  distill:
    output_dir: "distill/"
    keep_workspace: false
---
# Workflow prompt

Render with {{ issue.id }} on repository {{ repo.id }}.
"#
}

/// Construct a `WORKFLOW.md` whose schema is valid but uses a different value
/// for one of the typed fields, so a successful reload is observable.
fn alternate_valid_workflow() -> &'static str {
    r#"---
sandbox: read-only
elicitations: reject
max_turns: 50
extension:
  gates:
    spec:
      required_phases: ["requirements"]
---
# Updated body
"#
}

/// Construct a deliberately-invalid `WORKFLOW.md` that violates the schema at
/// a known key path.
fn invalid_workflow_bad_sandbox() -> &'static str {
    r#"---
sandbox: not-a-real-sandbox-mode
elicitations: reject
---
body
"#
}

#[test]
fn loads_valid_workflow_md_from_disk() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("WORKFLOW.md");
    std::fs::write(&path, valid_workflow_with_all_namespaces()).expect("write fixture");

    let policy = WorkflowLoader::load(&path).expect("valid file must load");
    assert_eq!(policy.max_turns, 25);
}

#[test]
fn extension_namespaces_round_trip_byte_for_byte_through_disk() {
    // Observable-completion #1 (integration half): all four reserved
    // sub-namespaces survive a round-trip through the on-disk loader.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("WORKFLOW.md");
    std::fs::write(&path, valid_workflow_with_all_namespaces()).expect("write fixture");

    let policy: WorkflowPolicy = WorkflowLoader::load(&path).expect("valid file must load");
    let ext = policy.extension_object().expect("extension is an object");

    // Reconstruct each reserved namespace and assert byte-for-byte equivalence
    // by comparing canonical-JSON serializations.
    let actual_spec = ext.get("gates").and_then(|g| g.get("spec")).expect("spec");
    let expected_spec = serde_json::json!({
        "required_phases": ["requirements", "design", "tasks"],
        "block_on_skipped": true,
    });
    assert_eq!(actual_spec, &expected_spec);

    let actual_review = ext
        .get("gates")
        .and_then(|g| g.get("review"))
        .expect("review");
    let expected_review = serde_json::json!({
        "block_on_findings": true,
        "reviewer_quorum": 2,
    });
    assert_eq!(actual_review, &expected_review);

    let actual_server = ext.get("server").expect("server");
    let expected_server = serde_json::json!({
        "bind": "127.0.0.1:7777",
        "static_root": "./public",
    });
    assert_eq!(actual_server, &expected_server);

    let actual_distill = ext.get("distill").expect("distill");
    let expected_distill = serde_json::json!({
        "output_dir": "distill/",
        "keep_workspace": false,
    });
    assert_eq!(actual_distill, &expected_distill);
}

#[tokio::test]
async fn hot_reload_replaces_policy_on_valid_change() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("WORKFLOW.md");
    std::fs::write(&path, valid_workflow_with_all_namespaces()).expect("write initial fixture");

    let mut handle = WorkflowLoader::watch(path.clone(), Duration::from_millis(150))
        .await
        .expect("watch must initialise from a valid file");

    let initial = handle.current();
    assert_eq!(initial.max_turns, 25);

    // Mutate the file to a different (still valid) policy. Use atomic rename
    // so editors and OS-level events behave like a real save.
    let staging = dir.path().join("WORKFLOW.md.tmp");
    std::fs::write(&staging, alternate_valid_workflow()).expect("write staging file");
    std::fs::rename(&staging, &path).expect("atomic rename");

    // Wait for the debounced reload to fire (timeout generously so CI noise
    // does not flake).
    timeout(Duration::from_secs(5), handle.changed())
        .await
        .expect("reload must arrive within timeout")
        .expect("watch channel must remain open");

    let updated = handle.current();
    assert_eq!(updated.max_turns, 50);
    assert_eq!(
        updated.sandbox,
        roki_daemon::workflow::SandboxMode::ReadOnly
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn hot_reload_retains_last_known_good_on_invalid_change() {
    // Observable-completion #2: an invalid edit must NOT replace the in-memory
    // policy and MUST emit a structured validation-failure log identifying
    // the offending key path.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("WORKFLOW.md");
    std::fs::write(&path, valid_workflow_with_all_namespaces()).expect("write initial fixture");

    let handle = WorkflowLoader::watch(path.clone(), Duration::from_millis(150))
        .await
        .expect("watch must initialise from a valid file");

    let good = handle.current();
    assert_eq!(good.max_turns, 25);

    // Mutate the file to be invalid (schema violation on `sandbox`).
    let staging = dir.path().join("WORKFLOW.md.tmp");
    std::fs::write(&staging, invalid_workflow_bad_sandbox()).expect("write invalid staging file");
    std::fs::rename(&staging, &path).expect("atomic rename to invalid contents");

    // Give the debouncer + reload pipeline time to react. We cannot block on
    // `handle.changed()` here because no successful reload should happen.
    let mut waited_ms = 0u64;
    let max_wait_ms = 5_000u64;
    let step_ms = 100u64;
    while waited_ms < max_wait_ms && !logs_contain("workflow_validation_failed") {
        tokio::time::sleep(Duration::from_millis(step_ms)).await;
        waited_ms += step_ms;
    }

    // Last-known-good fallback: the in-memory policy still equals the
    // originally-valid one we captured before mutating the file.
    let still = handle.current();
    assert_eq!(still.max_turns, good.max_turns);
    assert_eq!(still.sandbox, good.sandbox);

    // Structured log assertions: the warn event must include the
    // `workflow_validation_failed` event name and the offending key path.
    assert!(
        logs_contain("workflow_validation_failed"),
        "expected a structured validation-failure log event",
    );
    assert!(
        logs_contain("sandbox"),
        "expected the offending key path `sandbox` to appear in the log line",
    );
}
