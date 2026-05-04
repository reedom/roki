//! Integration smoke tests for the WORKFLOW.md loader and hot-reload watcher.
//!
//! In-file unit tests in `workflow::watcher::tests` cover the validate ->
//! retain-on-invalid path against synthetic policies. This file drives
//! `load_policy` directly against a real on-disk WORKFLOW.md tempfile and
//! asserts the canonical defaults are applied; a second test exercises the
//! watcher's last-known-good retention contract end-to-end.

use std::time::Duration;

use roki_daemon::shutdown;
use roki_daemon::workflow::schema::{DEFAULT_MAX_PHASES, DEFAULT_MODEL, Effort};
use roki_daemon::workflow::watcher::{load_policy, spawn_with_debounce};

const VALID_BODY: &str =
    "---\nextension:\n  orchestrator:\n    max_phases: 7\n---\n\
        ## prompt_template_orchestrator\norch v1\n\
        \n## prompt_template_implement_direct\nimpl\n\
        \n## prompt_template_validate_direct\nval\n\
        \n## prompt_template_open_pr\nopen\n";

const VALID_BODY_V2: &str =
    "---\nextension:\n  orchestrator:\n    max_phases: 12\n---\n\
        ## prompt_template_orchestrator\norch v2\n\
        \n## prompt_template_implement_direct\nimpl\n\
        \n## prompt_template_validate_direct\nval\n\
        \n## prompt_template_open_pr\nopen\n";

const INVALID_BODY: &str =
    "---\nextension:\n  orchestrator:\n    max_phases: 9999\n---\n\
        ## prompt_template_orchestrator\norch invalid\n\
        \n## prompt_template_implement_direct\nimpl\n\
        \n## prompt_template_validate_direct\nval\n\
        \n## prompt_template_open_pr\nopen\n";

#[tokio::test]
async fn load_policy_applies_canonical_defaults_to_minimal_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("WORKFLOW.md");
    std::fs::write(&path, VALID_BODY).unwrap();

    let policy = load_policy(&path).await.expect("load WORKFLOW.md");

    // The single declared key was `max_phases: 7`; everything else falls
    // through to the canonical defaults documented in
    // `workflow::schema::DEFAULT_*`.
    assert_eq!(policy.orchestrator.max_phases, 7);
    assert_eq!(policy.orchestrator.model, DEFAULT_MODEL);
    assert_eq!(policy.orchestrator.effort, Effort::Middle);

    // All four required template blocks must be present.
    for block in [
        "prompt_template_orchestrator",
        "prompt_template_implement_direct",
        "prompt_template_validate_direct",
        "prompt_template_open_pr",
    ] {
        assert!(
            policy.blocks.contains_key(block),
            "required block `{block}` missing from policy",
        );
    }
}

#[tokio::test]
async fn load_policy_with_no_overrides_uses_documented_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("WORKFLOW.md");
    std::fs::write(
        &path,
        "---\n---\n## prompt_template_orchestrator\nx\n\
         \n## prompt_template_implement_direct\nx\n\
         \n## prompt_template_validate_direct\nx\n\
         \n## prompt_template_open_pr\nx\n",
    )
    .unwrap();
    let policy = load_policy(&path).await.expect("load defaults");
    assert_eq!(policy.orchestrator.max_phases, DEFAULT_MAX_PHASES);
    assert_eq!(policy.orchestrator.model, DEFAULT_MODEL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hot_reload_retains_last_known_good_on_invalid_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("WORKFLOW.md");
    std::fs::write(&path, VALID_BODY).unwrap();

    let initial = load_policy(&path).await.expect("initial load");
    assert_eq!(initial.orchestrator.max_phases, 7);

    let (signal, trigger) = shutdown::new();
    let watcher = spawn_with_debounce(
        path.clone(),
        initial,
        signal,
        Duration::from_millis(50),
    )
    .await
    .expect("spawn watcher");

    // (a) Invalid change: previous policy retained.
    std::fs::write(&path, INVALID_BODY).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    {
        let guard = watcher.policy.read().await;
        assert_eq!(
            guard.orchestrator.max_phases, 7,
            "previous policy must be retained on invalid reload",
        );
    }

    // (b) Valid v2 change swaps the policy.
    std::fs::write(&path, VALID_BODY_V2).unwrap();
    let mut swapped = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let guard = watcher.policy.read().await;
        if guard.orchestrator.max_phases == 12 {
            swapped = true;
            break;
        }
        drop(guard);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(swapped, "valid reload must swap policy within timeout");

    trigger.fire();
    drop(watcher);
}
