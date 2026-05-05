//! Task 10.5 integration test: `runtime::testing::bootstrap_for_test`
//! exposes the paired `ShutdownTrigger` so e2e tests can wind the daemon
//! down without sending SIGINT/SIGTERM to the test harness process.
//!
//! The seam:
//!   1. Composes the runtime via `runtime::testing::bootstrap_for_test`,
//!      which mirrors the production `bootstrap` composition but returns
//!      the `ShutdownTrigger` instead of installing OS signal handlers.
//!   2. Spawns `serve()` on a tokio task.
//!   3. Fires the trigger.
//!   4. Awaits the spawned task within `SHUTDOWN_WINDOW + 1s`.
//!   5. Asserts `Ok(())`.
//!
//! Critically: no SIGINT / SIGTERM is delivered to the test harness
//! process; the trigger fires shutdown directly through the
//! `ShutdownSignal` watch channel.
//!
//! Spec refs: requirements.md Req 1.4; design.md "Daemon bootstrap" steps
//! 6 + 12.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use roki_daemon::cli::RunArgs;
use roki_daemon::config::StaticEnv;
use roki_daemon::runtime::testing::bootstrap_for_test;
use roki_daemon::runtime::{self, RuntimeError};
use roki_daemon::shutdown::SHUTDOWN_WINDOW;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_TOKEN: &str = "lin_api_token_for_trigger_seam";
const WEBHOOK_SECRET: &str = "webhook_secret_for_trigger_seam";

fn write_workflow(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("WORKFLOW.md");
    std::fs::write(
        &path,
        "---\nfoo: bar\n---\n\n## prompt_template_orchestrator\n\nbody\n\n## prompt_template_implement_direct\n\nbody\n\n## prompt_template_validate_direct\n\nbody\n\n## prompt_template_open_pr\n\nbody\n",
    )
    .unwrap();
    path
}

fn config_toml(workflow: &std::path::Path, endpoint: &str) -> String {
    format!(
        r#"
[linear]
api_token = {{ env = "LINEAR_API_TOKEN" }}
webhook_secret = {{ env = "LINEAR_WEBHOOK_SECRET" }}
assignee = "me"
endpoint = "{endpoint}"
poll_cadence_seconds = 300

[workflow]
path = "{}"

[server]
bind = "127.0.0.1"
port = 0

[permissions]
strategy = "settings-allowlist"
"#,
        workflow.display()
    )
}

fn env() -> StaticEnv {
    StaticEnv::new()
        .set("LINEAR_API_TOKEN", API_TOKEN)
        .set("LINEAR_WEBHOOK_SECRET", WEBHOOK_SECRET)
}

fn args(config: PathBuf) -> RunArgs {
    RunArgs {
        config: Some(config),
        bind: None,
        port: None,
        dangerously_skip_permissions: false,
        debug: false,
    }
}

/// Stand up a wiremock that satisfies the bootstrap viewer lookup + the
/// poller's first `list_issues` poll. The viewer mock is bounded so
/// production bootstrap can resolve a single Linear user id; the
/// `list_issues` mock returns an empty result so the poller can return to
/// its sleep loop without admitting any issues.
async fn linear_wiremock() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("viewer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "viewer": { "id": "u-trigger-seam" } }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issues": { "nodes": [] } }
        })))
        .mount(&server)
        .await;
    // The recovery scan (bootstrap step 8) walks the configured session
    // root for leftover per-issue directories; any host with cached
    // sessions from prior `roki` runs will surface them here, so the
    // wiremock must satisfy `issue(id:$id)` lookups too. Returning a
    // null `issue` makes recovery treat the row as `NoOp` and continue.
    Mock::given(method("POST"))
        .and(body_string_contains("issue("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issue": null }
        })))
        .mount(&server)
        .await;
    server
}

/// The seam test cannot drive `runtime::run` end-to-end on hosts that lack
/// the `wt`, `ghq`, or `claude` binaries on PATH; bootstrap step 7 hard-
/// refuses with the canonical `ExternalBinaryMissing` / `ClaudeBinary`
/// error in that case. Skip on those hosts so the test still runs in
/// environments without the prerequisites installed (per the established
/// pattern in `runtime_bind.rs`).
fn prerequisite_binaries_present() -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for name in ["wt", "ghq", "claude"] {
        let mut found = false;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if let Ok(meta) = std::fs::metadata(&candidate) {
                use std::os::unix::fs::PermissionsExt;
                if meta.is_file() && (meta.permissions().mode() & 0o111) != 0 {
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return false;
        }
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_trigger_seam_winds_serve_down_without_os_signal() {
    if !prerequisite_binaries_present() {
        eprintln!(
            "skipping shutdown_trigger_seam test: `wt` / `ghq` / `claude` missing on PATH"
        );
        return;
    }

    let server = linear_wiremock().await;

    let dir = TempDir::new().unwrap();
    let workflow = write_workflow(dir.path());
    let toml_path = dir.path().join("roki.toml");
    std::fs::write(&toml_path, config_toml(&workflow, &server.uri())).unwrap();

    let env = env();
    let (bootstrapped, trigger) = match bootstrap_for_test(args(toml_path), &env).await {
        Ok(pair) => pair,
        Err(RuntimeError::ExternalBinaryMissing { name }) => {
            eprintln!(
                "skipping shutdown_trigger_seam test: prerequisite binary `{name}` missing"
            );
            return;
        }
        Err(RuntimeError::ClaudeBinary(_)) => {
            eprintln!(
                "skipping shutdown_trigger_seam test: `claude` not discoverable in test environment"
            );
            return;
        }
        Err(other) => panic!("bootstrap_for_test failed: {other:?}"),
    };

    // Hold trigger via Arc<_> so the test fires from a separate task,
    // mirroring how the production signal handler task fires from outside
    // the serve() future.
    let trigger = Arc::new(trigger);

    // Spawn `serve()` on a task; the trigger fires shutdown without any
    // OS-signal delivery.
    let serve_handle = tokio::spawn(bootstrapped.serve());

    // Fire shortly after spawn so the webhook server, admission pipe, and
    // poller all have a chance to subscribe to the shared shutdown signal.
    tokio::time::sleep(Duration::from_millis(50)).await;
    trigger.fire();

    let started = Instant::now();
    let result = tokio::time::timeout(SHUTDOWN_WINDOW + Duration::from_secs(1), serve_handle)
        .await
        .expect("serve() must return inside SHUTDOWN_WINDOW + 1s");
    let elapsed = started.elapsed();

    let inner = result.expect("serve task panicked or was cancelled");
    inner.expect("serve() must return Ok after trigger fires");

    assert!(
        elapsed < SHUTDOWN_WINDOW + Duration::from_secs(1),
        "wind-down took {elapsed:?}, must be inside SHUTDOWN_WINDOW + 1s"
    );

    drop(server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_run_with_env_path_still_propagates_refusals() {
    // Smoke check: the seam refactor must not change `run_with_env`'s
    // observable refusal behavior — missing config still surfaces
    // `ConfigFileMissing` so production callers see the same error shape.
    let env = env();
    let path = std::env::temp_dir().join("roki-trigger-seam-missing.toml");
    let _ = std::fs::remove_file(&path);
    let err = runtime::run_with_env(args(path.clone()), &env)
        .await
        .unwrap_err();
    match err {
        RuntimeError::ConfigFileMissing { path: reported } => {
            assert_eq!(reported, path);
        }
        other => panic!("expected ConfigFileMissing, got {other:?}"),
    }
}
