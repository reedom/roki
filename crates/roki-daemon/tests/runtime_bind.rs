//! Webhook server bind tests for Task 10.3.
//!
//! Asserts the daemon refuses to start when the configured bind address is
//! already held by another process; the offending address must appear in the
//! actionable error message.

use std::net::TcpListener;
use std::path::PathBuf;

use roki_daemon::cli::RunArgs;
use roki_daemon::config::StaticEnv;
use roki_daemon::runtime::{self, RuntimeError};

fn env_with_secrets() -> StaticEnv {
    StaticEnv::new()
        .set("LINEAR_API_TOKEN", "lin_api_token_for_bind_test")
        .set("LINEAR_WEBHOOK_SECRET", "webhook_secret_for_bind_test")
}

/// Minimal valid `WORKFLOW.md` body with the four required template blocks.
/// Bootstrap step 8 now parses + validates the workflow before bind; the
/// stub body that worked under the readability-only check is no longer
/// sufficient.
const VALID_WORKFLOW_BODY: &str = "---\n---\n\
    ## prompt_template_orchestrator\norch body\n\n\
    ## prompt_template_implement_direct\nimpl body\n\n\
    ## prompt_template_validate_direct\nval body\n\n\
    ## prompt_template_open_pr\nopen body\n";

fn write_config_with_port(dir: &std::path::Path, host: &str, port: u16) -> PathBuf {
    let workflow = dir.join("WORKFLOW.md");
    std::fs::write(&workflow, VALID_WORKFLOW_BODY).unwrap();
    let toml = dir.join("roki.toml");
    let body = format!(
        r#"
[linear]
api_token = {{ env = "LINEAR_API_TOKEN" }}
webhook_secret = {{ env = "LINEAR_WEBHOOK_SECRET" }}
assignee = "me"

[workflow]
path = "{workflow}"

[server]
bind = "{host}"
port = {port}

[permissions]
strategy = "settings-allowlist"
"#,
        workflow = workflow.display(),
    );
    std::fs::write(&toml, body).unwrap();
    toml
}

#[tokio::test]
async fn port_conflict_refuses_with_offending_address_in_log() {
    // Reserve a port by binding a dummy listener up front, then point the
    // daemon's `[server]` block at the same address. The bootstrap must
    // refuse with `BindFailed { addr, .. }` whose message names the address.
    let dummy = TcpListener::bind("127.0.0.1:0").expect("dummy bind");
    let addr = dummy.local_addr().expect("dummy local_addr");

    let dir = tempfile::TempDir::new().unwrap();
    let toml = write_config_with_port(dir.path(), "127.0.0.1", addr.port());
    let env = env_with_secrets();

    let args = RunArgs {
        config: Some(toml),
        bind: None,
        port: None,
        dangerously_skip_permissions: false,
        debug: false,
    };

    let err = runtime::run_with_env(args, &env).await.unwrap_err();
    let msg = err.to_string();
    match &err {
        RuntimeError::BindFailed { addr: reported, .. } => {
            assert!(
                reported.contains(&addr.to_string()),
                "bind error must name offending address; got `{reported}` for `{addr}`"
            );
        }
        // The bootstrap may refuse before reaching the bind step on systems
        // without `wt`/`ghq` installed; if so, surface that explicitly so the
        // operator can install them and re-run.
        RuntimeError::ExternalBinaryMissing { name } => {
            eprintln!(
                "skipping port-conflict assertion: prerequisite binary `{name}` missing on PATH"
            );
            return;
        }
        RuntimeError::ClaudeBinary(_) => {
            eprintln!(
                "skipping port-conflict assertion: `claude` not discoverable in test environment"
            );
            return;
        }
        other => panic!("expected BindFailed, got {other:?} (msg: {msg})"),
    }
    assert!(msg.contains(&addr.to_string()), "{msg}");
    drop(dummy);
}

#[tokio::test]
async fn cli_port_override_replaces_config_port() {
    // CLI `--port` wins over `[server].port`. We cannot easily run the full
    // bootstrap to a successful bind in a test environment that may lack
    // `wt`/`ghq`, but we can assert the override applies via the same
    // port-conflict path: bind a dummy on a fresh port and pass it via
    // --port (with the config pointing at a different, free port).
    let dummy = TcpListener::bind("127.0.0.1:0").expect("dummy bind");
    let addr = dummy.local_addr().expect("dummy local_addr");

    let dir = tempfile::TempDir::new().unwrap();
    // Config points at a definitely-free port (0 = ephemeral) so the override
    // path is the only way the conflict is observable.
    let toml = write_config_with_port(dir.path(), "127.0.0.1", 0);
    let env = env_with_secrets();

    let args = RunArgs {
        config: Some(toml),
        bind: Some("127.0.0.1".to_owned()),
        port: Some(addr.port()),
        dangerously_skip_permissions: false,
        debug: false,
    };

    let err = runtime::run_with_env(args, &env).await.unwrap_err();
    match &err {
        RuntimeError::BindFailed { addr: reported, .. } => {
            assert!(
                reported.contains(&addr.port().to_string()),
                "expected reported addr to contain overridden port; got `{reported}`"
            );
        }
        RuntimeError::ExternalBinaryMissing { name } => {
            eprintln!(
                "skipping override assertion: prerequisite binary `{name}` missing on PATH"
            );
        }
        RuntimeError::ClaudeBinary(_) => {
            eprintln!(
                "skipping override assertion: `claude` not discoverable in test environment"
            );
        }
        other => panic!("expected BindFailed, got {other:?}"),
    }
    drop(dummy);
}
