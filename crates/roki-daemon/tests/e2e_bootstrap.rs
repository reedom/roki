//! End-to-end bootstrap smoke test (task 5.1).
//!
//! Drives `runtime::run_with_shutdown` against a synthesised config that
//! points at a wiremock Linear server, mounts a per-repo webhook route, and
//! supervises the `fake_claude` example binary. The test asserts that:
//!
//! 1. The bootstrap composes every component end-to-end (config → logging →
//!    workflow loader → workspace → orchestrator → tracker pollers → axum
//!    webhook server) without panicking.
//! 2. The axum server actually binds the configured port — the test connects
//!    a TCP socket to the bound address.
//! 3. A correctly-signed Linear webhook posted to `/linear/webhook/<repo-id>`
//!    is accepted (HTTP 204) and forwarded into the orchestrator.
//! 4. The orchestrator drives the issue through the documented happy-path
//!    transition prefix (`Discovered -> Queued -> Active ->
//!    AwaitingReview`).
//! 5. Triggering the externally-owned `ShutdownSignal` causes
//!    `run_with_shutdown` to return `Ok(())` within the documented 30s
//!    shutdown window.
//!
//! Determinism notes:
//!
//! * Secrets (Linear token + webhook HMAC secret) live in tempfiles or in
//!   the literal `webhook_secret` config field. The workspace lint
//!   `unsafe_code = "forbid"` blocks `std::env::set_var`, so the test
//!   sidesteps env-var mutation entirely.
//! * Server port is discovered via `TcpListener::bind("127.0.0.1:0")` and
//!   then released; the bootstrap may race another binder, but the window
//!   is small for a single-threaded test.
//! * `fake_claude` defaults to `clean_exit` mode — exactly what the
//!   orchestrator promotes from `Active -> AwaitingReview`.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use roki_daemon::cli::RunArgs;
use roki_daemon::orchestrator::events::{SubscriberError, TransitionSubscriber};
use roki_daemon::orchestrator::state::{TransitionEvent, WorkerState};
use roki_daemon::runtime::{BootstrapHandles, run_with_shutdown};
mod common;
use roki_daemon::shutdown::ShutdownSignal;
use serde_json::{Value, json};

type HmacSha256 = Hmac<Sha256>;

const TEST_REPO_ID: &str = "core";
const TEST_ISSUE_ID: &str = "ENG-1";
const TEST_WEBHOOK_SECRET: &str = "bootstrap-smoke-secret-fixed";
const TEST_LINEAR_TOKEN: &str = "lin_e2e_bootstrap_token";

/// Build the `fake_claude` example once per `cargo test` invocation and
/// return its absolute path.
fn fake_claude_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = StdCommand::new(&cargo)
            .args(["build", "--example", "fake_claude"])
            .status()
            .expect("must be able to invoke `cargo build --example fake_claude`");
        assert!(
            status.success(),
            "`cargo build --example fake_claude` failed with {status:?}",
        );
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest
            .parent()
            .and_then(|p| p.parent())
            .expect("CARGO_MANIFEST_DIR must have a workspace ancestor")
            .to_path_buf();
        let bin = workspace_root
            .join("target")
            .join("debug")
            .join("examples")
            .join(if cfg!(windows) {
                "fake_claude.exe"
            } else {
                "fake_claude"
            });
        assert!(
            bin.exists(),
            "fake_claude binary missing at {}",
            bin.display(),
        );
        bin
    })
}

fn pick_free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

fn started_payload() -> Value {
    json!({
        "data": {
            "issues": {
                "nodes": [
                    {
                        "id": "uuid-1",
                        "identifier": TEST_ISSUE_ID,
                        "title": "bootstrap smoke",
                        "description": "drive the daemon end-to-end",
                        "state": { "type": "started", "name": "In Progress" },
                        "labels": { "nodes": [] },
                        "team": { "key": "ENG" }
                    }
                ]
            }
        }
    })
}

/// Linear webhook envelope that mirrors the fixture used by
/// `tests/tracker_webhook.rs` so the bootstrap path exercises the same
/// post-signature decode shape the receiver was tested against in 2.6.
fn webhook_envelope() -> Value {
    json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": "uuid-here",
            "identifier": TEST_ISSUE_ID,
            "title": "bootstrap smoke",
            "description": "Body text",
            "state": { "type": "started", "name": "In Progress" },
            "team": { "key": "ENG" },
            "labels": { "nodes": [ { "name": "bug" } ] }
        }
    })
}

fn minimal_workflow() -> &'static str {
    "---\n\
sandbox: workspace-write\n\
elicitations: reject\n\
max_turns: 30\n\
stall_window_seconds: 120\n\
backoff:\n  min_seconds: 10\n  max_seconds: 300\n\
---\n\
# Bootstrap workflow\n\
Render with {{ issue.id }}.\n"
}

fn write_fixtures(
    server_uri: &str,
    workflow_path: &Path,
    config_path: &Path,
    linear_token_path: &Path,
    bind_port: u16,
) {
    std::fs::write(workflow_path, minimal_workflow()).expect("write workflow");
    std::fs::write(linear_token_path, TEST_LINEAR_TOKEN).expect("write linear token");

    let claude_binary = fake_claude_path().to_str().expect("utf-8 fake_claude path");
    let workflow_path_str = workflow_path.to_str().expect("utf-8 workflow path");
    let token_file_str = linear_token_path.to_str().expect("utf-8 token file");

    // The bootstrap will resolve the repo path at runtime via `ghq`. Tests
    // that ride `runtime::run_with_shutdown` therefore require `wt` and
    // `ghq` on PATH AND a real ghq-managed checkout for `owner/{repo}`.
    // TODO(7.1f): post-7.1a the workspace-level webhook secret is resolved
    // from `[linear].webhook_secret_env`, and the workspace-level workflow
    // policy from `[workflow].path`. Setting an env var here is blocked by
    // the crate's `unsafe_code = "forbid"` lint, so this fixture writes the
    // new shape but the test itself is currently `#[ignore]`-d until the
    // bootstrap is reshimmed by 7.1f to accept a test-injectable secret.
    let _ = TEST_WEBHOOK_SECRET; // silence dead-code lints until 7.1f.
    let toml = format!(
        r#"
polling_cadence_seconds = 60
max_concurrent_workers = 1
claude_binary = "{claude_binary}"

[server]
bind = "127.0.0.1"
port = {bind_port}

[linear]
token_file = "{token_file_str}"
endpoint = "{server_uri}/graphql"
webhook_secret_env = "ROKI_BOOTSTRAP_TEST_WEBHOOK_SECRET"

[workflow]
path = "{workflow_path_str}"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "owner/{TEST_REPO_ID}"
"#
    );
    std::fs::write(config_path, toml).expect("write config.toml");
}

/// Skip the bootstrap smoke test when `wt` or `ghq` is not on PATH. The
/// task-6.1 bootstrap refuses to start without both, and the test cannot
/// substitute a mock through the public bootstrap API. Returns `true`
/// when the test should proceed, `false` to skip with a recognisable
/// log line.
fn external_tools_present() -> bool {
    let wt = std::process::Command::new("wt")
        .arg("--version")
        .output()
        .is_ok();
    let ghq = std::process::Command::new("ghq")
        .arg("--version")
        .output()
        .is_ok();
    wt && ghq
}

/// True when this CI host is equipped to run the bootstrap smoke test
/// end-to-end. The test requires (a) `wt` and `ghq` on PATH and (b) a
/// pre-existing `owner/<repo_id>` checkout discoverable via
/// `ghq list -p`. Operators preparing CI must run
/// `git init && git commit --allow-empty -m seed` under
/// `<ghq_root>/github.com/owner/<repo_id>` to satisfy (b); the test
/// silently skips when the prerequisite is absent so a developer who has
/// never opted into the heavy bootstrap fixture is not blocked by an
/// environment they can't satisfy.
fn bootstrap_prerequisites_ready(repo_id: &str) -> bool {
    if !external_tools_present() {
        return false;
    }
    let identifier = format!("owner/{repo_id}");
    let output = match std::process::Command::new("ghq")
        .args(["list", "-p", identifier.as_str()])
        .output()
    {
        Ok(out) => out,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    PathBuf::from(trimmed).is_dir()
}

#[derive(Default)]
struct RecordedTransitions {
    log: Mutex<Vec<TransitionEvent>>,
}

#[async_trait]
impl TransitionSubscriber for RecordedTransitions {
    fn id(&self) -> &str {
        "e2e-bootstrap-recorder"
    }

    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        self.log.lock().await.push(event.clone());
        Ok(())
    }
}

async fn await_cond<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    while !cond() {
        if timeout <= start.elapsed() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

async fn wait_for_port_ready(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn hmac_hex(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac init");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[tokio::test]
#[ignore = "TODO(7.1f): bootstrap requires the workspace-level webhook secret \
            env-var, but the crate's unsafe_code lint forbids std::env::set_var \
            in tests. Re-enable when 7.1f reshims the bootstrap with a \
            test-injectable secret seam."]
async fn bootstrap_drives_issue_through_documented_happy_path() {
    if !bootstrap_prerequisites_ready(TEST_REPO_ID) {
        eprintln!(
            "skipping bootstrap smoke test: requires `wt`/`ghq` on PATH and a pre-existing `owner/{TEST_REPO_ID}` ghq checkout (see test docstring for setup)",
        );
        return;
    }
    // ---- Fake Linear ---------------------------------------------------
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(started_payload()))
        .mount(&server)
        .await;

    // ---- Tempdirs + config file ---------------------------------------
    let workflow_dir = TempDir::new().expect("workflow tempdir");
    let workflow_path = workflow_dir.path().join("WORKFLOW.md");

    let config_dir = TempDir::new().expect("config tempdir");
    let config_path = config_dir.path().join("roki.toml");
    let linear_token_path = config_dir.path().join("linear-token");

    let bind_port = pick_free_port();
    write_fixtures(
        &server.uri(),
        &workflow_path,
        &config_path,
        &linear_token_path,
        bind_port,
    );

    // ---- runtime::run_with_shutdown spawn ------------------------------
    let shutdown = ShutdownSignal::new();
    let args = RunArgs {
        config: Some(config_path.clone()),
        bind: None,
        port: None,
        dangerously_skip_permissions: false,
    };
    let (handles_tx, handles_rx) = oneshot::channel::<BootstrapHandles>();
    let run_shutdown = shutdown.clone();
    let run_handle =
        tokio::spawn(async move { run_with_shutdown(args, run_shutdown, Some(handles_tx)).await });

    // ---- Wait for bootstrap handles + readiness ------------------------
    let handles = tokio::time::timeout(Duration::from_secs(15), handles_rx)
        .await
        .expect("bootstrap must publish handles within 15s")
        .expect("bootstrap handles channel must not close");

    // The bootstrap publishes the actual bound port so a TOCTOU race against
    // another binder shows up here as a mismatch, not as a port conflict.
    assert_eq!(
        handles.bind_port, bind_port,
        "bootstrap must report the configured port",
    );

    // Register the recorder on the EventBus before driving any transitions.
    let recorder = Arc::new(RecordedTransitions::default());
    handles.event_bus.register(recorder.clone());

    // The axum server must accept a TCP connection within the readiness window.
    let port_ready = wait_for_port_ready(bind_port, Duration::from_secs(10)).await;
    assert!(
        port_ready,
        "axum server must bind {bind_port} within readiness window",
    );

    // ---- Post a signed Linear webhook ----------------------------------
    let url = format!("http://127.0.0.1:{bind_port}/linear/webhook/{TEST_REPO_ID}");
    let body_bytes = serde_json::to_vec(&webhook_envelope()).expect("encode envelope");
    let signature = hmac_hex(TEST_WEBHOOK_SECRET.as_bytes(), &body_bytes);

    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .header("Linear-Signature", signature)
        .body(body_bytes)
        .send()
        .await
        .expect("webhook POST must reach the daemon");
    assert_eq!(
        response.status().as_u16(),
        204,
        "signed webhook must be accepted with HTTP 204",
    );

    // ---- Wait for the orchestrator to reach AwaitingReview -------------
    let recorder_for_check = recorder.clone();
    let reached = await_cond(Duration::from_secs(20), || {
        let log = recorder_for_check.log.try_lock();
        match log {
            Ok(entries) => entries
                .iter()
                .any(|ev| ev.next == WorkerState::AwaitingReview),
            Err(_) => false,
        }
    })
    .await;
    assert!(
        reached,
        "actor must reach AwaitingReview; recorded so far: {:?}",
        recorder.log.lock().await,
    );

    // The documented prefix of the happy-path sequence must be present in
    // order. The suffix (`AwaitingReview -> TerminalSuccess -> Cleaning`)
    // requires the tracker to flip to `completed`, which the smoke test
    // does not orchestrate.
    let log = recorder.log.lock().await.clone();
    let pairs: Vec<(WorkerState, WorkerState)> =
        log.iter().map(|ev| (ev.previous, ev.next)).collect();
    let expected_prefix = [
        (WorkerState::Discovered, WorkerState::Queued),
        (WorkerState::Queued, WorkerState::Active),
        (WorkerState::Active, WorkerState::AwaitingReview),
    ];
    assert!(
        expected_prefix.len() <= pairs.len(),
        "bootstrap must commit at least the happy-path prefix; got {pairs:?}",
    );
    assert_eq!(
        &pairs[..expected_prefix.len()],
        &expected_prefix[..],
        "bootstrap-driven transitions must match the documented happy-path prefix",
    );
    for ev in &log {
        assert_eq!(ev.issue.as_str(), TEST_ISSUE_ID);
    }

    // ---- Trigger shutdown ---------------------------------------------
    shutdown.trigger();
    let result = tokio::time::timeout(Duration::from_secs(30), run_handle)
        .await
        .expect("run_with_shutdown must return within the 30s shutdown window")
        .expect("run task must not panic");
    assert!(
        result.is_ok(),
        "runtime::run_with_shutdown must return Ok(()) on clean shutdown; got {result:?}",
    );
}
