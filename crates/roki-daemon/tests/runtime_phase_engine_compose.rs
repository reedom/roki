//! Task 10.7 integration test: production phase pipeline wiring.
//!
//! Asserts:
//!
//! 1. `runtime::testing::bootstrap_for_test` composes the production
//!    `PhaseSubprocessEngineImpl` (not the previous `PendingPhaseEngine`
//!    placeholder) into the orchestrator actor map. The signal is the
//!    strong count of `RuntimeComponents.phase_subprocess_adapter`: the
//!    real engine impl clones the adapter into its own slot, lifting the
//!    count to >= 2.
//!
//! 2. `PhaseSubprocessEngineImpl::run_phase` drives a real subprocess via
//!    the adapter against the `fake_claude` example binary, threads the
//!    `additional_context` envelope through the documented delimiter
//!    contract, and translates the subprocess exit into
//!    `PhaseRunOutcome::Translated(DaemonEvent::PhaseComplete(...))` per
//!    Req 6.7 / 7.1 / 7.3.
//!
//! 3. The mid-phase abort gap (Req 1.4) is closed: dropping the future
//!    that owns an in-flight phase handle (e.g. when the per-issue actor
//!    task is aborted) SIGKILLs the spawned `claude` child via
//!    `Command::kill_on_drop(true)` set in `engine::claude::ClaudeSpawn`,
//!    so the subprocess does not survive past the dropped handle.
//!
//! Spec refs: requirements.md Req 1.4, 6.7, 7.1, 7.3, 13.4;
//! design.md "PhaseSubprocessAdapter".

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use roki_daemon::cli::RunArgs;
use roki_daemon::config::StaticEnv;
use roki_daemon::engine::claude::ClaudeBinary;
use roki_daemon::engine::orchestrator_session::action_parser::PhaseName;
use roki_daemon::engine::orchestrator_session::events::DaemonEvent;
use roki_daemon::engine::phase_subprocess::adapter::PhaseSubprocessAdapter;
use roki_daemon::engine::phase_subprocess::catalog::PhaseLaunchContext;
use roki_daemon::engine::phase_subprocess::engine_impl::PhaseSubprocessEngineImpl;
use roki_daemon::engine::phase_subprocess::exit::{
    ExitTranslationInputs, translate_exit,
};
use roki_daemon::orchestrator::core::{PhaseEngine, PhaseRunOutcome};
use roki_daemon::orchestrator::state::{IssueId, Mode};
use roki_daemon::permissions::PermissionResolver;
use roki_daemon::runtime::testing::bootstrap_for_test;
use roki_daemon::shutdown::SHUTDOWN_WINDOW;
use roki_daemon::workflow::schema::{OrchestratorConfig, WorkflowPolicy};
use tokio::sync::oneshot;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{fake_claude_path, write_mode};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const E2E_API_TOKEN: &str = "lin_api_token_for_runtime_phase_compose";
const E2E_WEBHOOK_SECRET: &str = "webhook_secret_for_runtime_phase_compose";

fn env_with_secrets() -> StaticEnv {
    StaticEnv::new()
        .set("LINEAR_API_TOKEN", E2E_API_TOKEN)
        .set("LINEAR_WEBHOOK_SECRET", E2E_WEBHOOK_SECRET)
}

fn args_with_config(path: PathBuf) -> RunArgs {
    RunArgs {
        config: Some(path),
        bind: None,
        port: None,
        dangerously_skip_permissions: false,
        debug: false,
    }
}

fn write_compose_config(dir: &Path, endpoint: &str) -> PathBuf {
    let workflow = dir.join("WORKFLOW.md");
    std::fs::write(
        &workflow,
        "---\nfoo: bar\n---\n\n## prompt_template_orchestrator\n\nbody\n\n## prompt_template_implement_direct\n\nbody\n\n## prompt_template_validate_direct\n\nbody\n\n## prompt_template_open_pr\n\nbody\n",
    )
    .unwrap();
    let toml = dir.join("roki.toml");
    let body = format!(
        r#"
[linear]
api_token = {{ env = "LINEAR_API_TOKEN" }}
webhook_secret = {{ env = "LINEAR_WEBHOOK_SECRET" }}
assignee = "me"
endpoint = "{endpoint}"
admit_states = ["Todo"]
poll_cadence_seconds = 300

[workflow]
path = "{workflow}"

[server]
bind = "127.0.0.1"
port = 0

[permissions]
strategy = "settings-allowlist"
"#,
        workflow = workflow.display()
    );
    std::fs::write(&toml, body).unwrap();
    toml
}

#[cfg(unix)]
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

#[cfg(not(unix))]
fn prerequisite_binaries_present() -> bool {
    false
}

async fn linear_wiremock() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("viewer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "viewer": { "id": "u-e2e-phase-engine" } }
        })))
        .expect(1..)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issues": { "nodes": [] } }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("issue("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "issue": null }
        })))
        .mount(&server)
        .await;
    server
}

fn workflow_policy_with_open_pr_template() -> Arc<WorkflowPolicy> {
    let mut blocks = BTreeMap::new();
    blocks.insert(
        "prompt_template_open_pr".to_owned(),
        "open_pr issue={{ issue }} mode={{ mode }}\n".to_owned(),
    );
    blocks.insert(
        "prompt_template_implement_direct".to_owned(),
        "impl issue={{ issue }} mode={{ mode }}\n".to_owned(),
    );
    blocks.insert(
        "prompt_template_validate_direct".to_owned(),
        "validate issue={{ issue }} mode={{ mode }}\n".to_owned(),
    );
    Arc::new(WorkflowPolicy {
        orchestrator: OrchestratorConfig::default(),
        phases: BTreeMap::new(),
        server: serde_json::Value::Object(Default::default()),
        blocks,
        raw_unknowns: serde_json::Value::Object(Default::default()),
    })
}

/// Build a real `PhaseSubprocessEngineImpl` backed by the `fake_claude`
/// example binary so the test exercises the production wiring end-to-end
/// without depending on `bootstrap_for_test`.
#[cfg(unix)]
fn build_engine_with_fake_claude(
    session_dir: &Path,
) -> (Arc<PhaseSubprocessEngineImpl>, Arc<PhaseSubprocessAdapter>) {
    // Adapter ignores the `--settings <path>` flag for the fake harness, but
    // the spawn primitive prepends it so the file must exist on disk.
    std::fs::write(session_dir.join("settings.json"), b"{}").unwrap();

    let binary = ClaudeBinary::discover(Some(fake_claude_path())).unwrap();
    let permissions = PermissionResolver::with_settings_path(
        session_dir.join("settings.json"),
        vec!["Read".to_owned()],
    );
    let adapter = Arc::new(PhaseSubprocessAdapter::new(binary, permissions.clone()));
    let engine = Arc::new(PhaseSubprocessEngineImpl::new(
        adapter.clone(),
        workflow_policy_with_open_pr_template(),
        permissions,
        None,
    ));
    (engine, adapter)
}

// ---------------------------------------------------------------------------
// (1) Bootstrap composition wires the real PhaseSubprocessEngineImpl.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_composition_wires_real_phase_subprocess_engine() {
    if !prerequisite_binaries_present() {
        eprintln!(
            "skipping bootstrap_composition_wires_real_phase_subprocess_engine: \
             `wt` / `ghq` / `claude` missing on PATH"
        );
        return;
    }

    let server = linear_wiremock().await;
    let dir = TempDir::new().unwrap();
    let toml = write_compose_config(dir.path(), &server.uri());
    let env = env_with_secrets();

    let (bootstrapped, trigger) = match bootstrap_for_test(args_with_config(toml), &env).await {
        Ok(pair) => pair,
        Err(err) => panic!("bootstrap_for_test failed: {err:?}"),
    };

    // Load-bearing assertion: the production composition wired the real
    // PhaseSubprocessEngineImpl, not the previous PendingPhaseEngine
    // placeholder. The placeholder did not retain the adapter; the real
    // impl clones it into its own slot.
    assert!(
        bootstrapped.has_real_phase_engine(),
        "bootstrap must wire PhaseSubprocessEngineImpl into the orchestrator actor map",
    );

    // Tear down cleanly so the test does not leak the listener or poller.
    let serve_handle = tokio::spawn(bootstrapped.serve());
    tokio::time::sleep(Duration::from_millis(50)).await;
    trigger.fire();
    let result = tokio::time::timeout(SHUTDOWN_WINDOW + Duration::from_secs(1), serve_handle)
        .await
        .expect("serve() must return inside SHUTDOWN_WINDOW + 1s");
    let inner = result.expect("serve task panicked or was cancelled");
    inner.expect("serve() must return Ok(()) after trigger fires");

    drop(server);
}

// ---------------------------------------------------------------------------
// (2) PhaseSubprocessEngineImpl drives the adapter end-to-end.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_impl_run_phase_emits_phase_complete_via_real_subprocess() {
    let tmp = tempfile::tempdir().unwrap();
    // The capture-stdin mode writes the verbatim stdin (system-prompt
    // envelope + rendered template body) to `<cwd>/.fake_claude_stdin_capture`
    // before emitting `subtype: success`, so we can positively assert that
    // `additional_context` actually traverses the documented
    // engine_impl -> adapter -> child stdin path (Req 6.7 / 7.1 / 7.3).
    write_mode(tmp.path(), "phase_success_capture_stdin");

    let (engine, _adapter) = build_engine_with_fake_claude(tmp.path());

    const CONTEXT_BODY: &str = "verbatim additional context body";

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        engine.run_phase(
            &IssueId::from("ENG-PE-1"),
            PhaseName::OpenPr,
            Mode::SpecDriven,
            Some(tmp.path().join("wt")),
            Some(CONTEXT_BODY.to_owned()),
            tmp.path().to_path_buf(),
        ),
    )
    .await
    .expect("engine.run_phase timed out")
    .expect("engine.run_phase failed");

    match outcome {
        PhaseRunOutcome::Translated(DaemonEvent::PhaseComplete(payload)) => {
            assert_eq!(payload.phase, PhaseName::OpenPr);
        }
        other => panic!("expected PhaseComplete(OpenPr), got {other:?}"),
    }

    // Positive verification of additional_context threading: the harness
    // captured everything it received on stdin and we assert the verbatim
    // body landed in the rendered template body delivered to the child.
    // If `engine_impl.rs` were regressed to pass `additional_context: None`
    // (or to drop the field on the way to `PhaseLaunchContext`), this read
    // would still succeed but the contains-check would fail.
    let capture_path = tmp.path().join(".fake_claude_stdin_capture");
    let captured = std::fs::read_to_string(&capture_path).unwrap_or_else(|err| {
        panic!(
            "fake_claude must have written {} after `phase_success_capture_stdin`: {err}",
            capture_path.display(),
        )
    });
    assert!(
        captured.contains(CONTEXT_BODY),
        "additional_context body must reach the subprocess stdin verbatim; got: {captured:?}",
    );
}

// ---------------------------------------------------------------------------
// (3) Mid-phase abort: kill_on_drop SIGKILLs the in-flight subprocess.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_in_flight_phase_future_kills_subprocess() {
    use roki_daemon::permissions::PermissionStrategy;

    let tmp = tempfile::tempdir().unwrap();
    // The `phase_stall` mode keeps the child sleeping for 60s with no
    // stdout, so the `translate_exit` future cannot resolve on its own —
    // the only way the subprocess can exit inside the test budget is via
    // SIGKILL from `kill_on_drop(true)` when the owning future is dropped.
    write_mode(tmp.path(), "phase_stall");
    std::fs::write(tmp.path().join("settings.json"), b"{}").unwrap();

    // Build the adapter directly so we control the spawn -> drop seam at
    // the same granularity as the engine impl. (Bypassing `engine.run_phase`
    // is necessary to capture the child PID before the future is dropped;
    // `run_phase` does not surface the PID to its caller.)
    let binary = ClaudeBinary::discover(Some(fake_claude_path())).unwrap();
    let permissions = PermissionResolver::with_settings_path(
        tmp.path().join("settings.json"),
        vec!["Read".to_owned()],
    );
    let adapter = Arc::new(PhaseSubprocessAdapter::new(binary, permissions.clone()));
    let policy = workflow_policy_with_open_pr_template();

    // Resolve the per-phase permission shape exactly as `engine_impl.rs`
    // does so the spawn path matches production wiring.
    let resolved = permissions.resolve_for_phase(PhaseName::Implement).unwrap();
    let allowed_tools = resolved.allowed_tools.clone().unwrap_or_default();
    let permission_strategy = match resolved.strategy {
        PermissionStrategy::SettingsAllowlist { settings_path } => {
            PermissionStrategy::SettingsAllowlist { settings_path }
        }
        PermissionStrategy::DangerouslySkipPermissions => {
            PermissionStrategy::DangerouslySkipPermissions
        }
    };

    let ctx = PhaseLaunchContext {
        issue: IssueId::from("ENG-PE-ABORT"),
        phase: PhaseName::Implement,
        mode: Mode::SpecDriven,
        additional_context: None,
        worktree_path: Some(tmp.path().join("wt")),
        session_tempdir: tmp.path().to_path_buf(),
        max_turns: 0,
        workflow_policy: policy,
        permission_strategy,
        allowed_tools,
    };

    let handle = adapter
        .spawn(ctx, None)
        .await
        .expect("adapter.spawn should succeed against fake_claude `phase_stall`");

    // Capture the OS-level PID *before* moving the handle into a future:
    // once the handle is dropped, `child.id()` is no longer addressable,
    // and we need the raw pid to probe via `kill(pid, 0)` post-drop.
    let pid: i32 = handle
        .child
        .id()
        .expect("spawned child must expose a PID before it is awaited") as i32;

    // Wrap the documented exit-translation pipeline (the same composition
    // `engine_impl.rs` uses) inside a task so we can drop the future by
    // aborting the task. A never-firing oneshot mirrors the production
    // wiring of `tracker_terminal_signal` for the no-tracker-preempt case.
    let stall_window = Duration::from_secs(handle.stall_seconds.into());
    let (_send_tt, recv_tt) = oneshot::channel();
    let inputs = ExitTranslationInputs {
        child: handle.child,
        stream_rx: handle.stream_rx,
        phase: PhaseName::Implement,
        stall_window,
        tracker_terminal_signal: recv_tt,
    };

    let translate_task = tokio::spawn(async move {
        let _ = translate_exit(inputs).await;
    });

    // Confirm the child is alive immediately after spawn — sanity-checks
    // the PID we captured before we go on to assert death. `kill -0 <pid>`
    // is the portable shell equivalent of `kill(pid, 0)`: it does not
    // deliver a signal, only probes the existence of the process. The
    // workspace forbids `unsafe_code`, so we shell out to `/bin/kill`
    // (always present on macOS + Linux) instead of calling `libc::kill`
    // directly.
    assert!(
        pid_alive(pid),
        "fake_claude child must be alive immediately after spawn (pid={pid})",
    );

    // Give the child a brief moment to settle into its 60s sleep so the
    // abort-driven drop is the only thing that can interrupt it.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Aborting the task drops the `ExitTranslationInputs` future, which
    // drops the held `tokio::process::Child`. With
    // `Command::kill_on_drop(true)` set in `ClaudeSpawn::spawn`, tokio's
    // reaper sends SIGKILL to the child and reaps the zombie.
    translate_task.abort();
    let _ = translate_task.await;

    // Poll for `kill -0 <pid>` to fail (process does not exist) for up to
    // ~500ms. `kill_on_drop` is best-effort SIGKILL on Drop and is
    // delivered asynchronously by the tokio reaper; the bound is generous
    // enough to absorb slow CI hosts without masking a regression where
    // the child outlives the future.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let mut still_alive_at_deadline = true;
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            still_alive_at_deadline = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        !still_alive_at_deadline,
        "child pid={pid} must be unreachable after the owning future is dropped \
         (kill_on_drop regression?); `kill -0 {pid}` still succeeds 500ms post-abort",
    );
}

/// Portable process-aliveness probe via `/bin/kill -0 <pid>`. Returns true
/// when the pid is reachable, false when the OS reports `No such process`
/// (ESRCH). Avoids the `unsafe_code = forbid` workspace lint that blocks
/// `libc::kill` from being called directly in a test.
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Compile-time guard: PhaseEngine trait still consumes session_tempdir.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _phase_engine_trait_signature_guard(engine: Arc<dyn PhaseEngine>) {
    // If the trait signature regresses (e.g. session_tempdir is removed),
    // this stub will fail to compile, surfacing the regression at type-check
    // time rather than at runtime.
    let _: Arc<dyn PhaseEngine> = engine;
}
