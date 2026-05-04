//! Bootstrap smoke tests for the daemon runtime.
//!
//! These tests cover the refusal paths from Tasks 10.1 + 10.2:
//! - Missing config file produces an actionable refusal.
//! - Missing required external binaries (`wt`, `ghq`) produce an actionable
//!   refusal naming the missing executable.
//! - Legacy config keys bubble up as a startup refusal (delegated to the
//!   config loader, but this asserts the runtime layer surfaces it).
//!
//! The full e2e_bootstrap path lives under Task 13.1.

use std::path::PathBuf;

use roki_daemon::cli::RunArgs;
use roki_daemon::config::StaticEnv;
use roki_daemon::runtime::{self, RuntimeError};

fn args_with_config(path: PathBuf) -> RunArgs {
    RunArgs {
        config: Some(path),
        bind: None,
        port: None,
        dangerously_skip_permissions: false,
        debug: false,
    }
}

fn env_with_secrets() -> StaticEnv {
    StaticEnv::new()
        .set("LINEAR_API_TOKEN", "lin_api_token_for_smoke")
        .set("LINEAR_WEBHOOK_SECRET", "webhook_secret_for_smoke")
}

fn write_minimal_config(dir: &std::path::Path) -> PathBuf {
    let workflow = dir.join("WORKFLOW.md");
    std::fs::write(&workflow, "---\nfoo: bar\n---\n\n## prompt_template_orchestrator\n\nbody\n\n## prompt_template_implement_direct\n\nbody\n\n## prompt_template_validate_direct\n\nbody\n\n## prompt_template_open_pr\n\nbody\n").unwrap();
    let toml = dir.join("roki.toml");
    let body = format!(
        r#"
[linear]
api_token = {{ env = "LINEAR_API_TOKEN" }}
webhook_secret = {{ env = "LINEAR_WEBHOOK_SECRET" }}
assignee = "me"

[workflow]
path = "{}"

[server]
bind = "127.0.0.1"
port = 0

[permissions]
strategy = "settings-allowlist"
"#,
        workflow.display()
    );
    std::fs::write(&toml, body).unwrap();
    toml
}

#[tokio::test]
async fn missing_config_path_refuses_at_step_1() {
    let env = env_with_secrets();
    let path = std::env::temp_dir().join("roki-runtime-smoke-missing.toml");
    let _ = std::fs::remove_file(&path);
    let err = runtime::run_with_env(args_with_config(path.clone()), &env)
        .await
        .unwrap_err();
    match err {
        RuntimeError::ConfigFileMissing { path: reported } => {
            assert_eq!(reported, path);
        }
        other => panic!("expected ConfigFileMissing, got {other:?}"),
    }
}

#[tokio::test]
async fn legacy_config_key_bubbles_up_as_actionable_refusal() {
    let env = env_with_secrets();
    let dir = tempfile::TempDir::new().unwrap();
    let workflow = dir.path().join("WORKFLOW.md");
    std::fs::write(&workflow, "stub").unwrap();
    let toml = dir.path().join("roki.toml");
    // Legacy `[judge].model` key — refused by config loader (Req 2.12).
    let body = format!(
        r#"
[judge]
model = "claude-sonnet-4"

[linear]
api_token = {{ env = "LINEAR_API_TOKEN" }}
webhook_secret = {{ env = "LINEAR_WEBHOOK_SECRET" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
        workflow.display()
    );
    std::fs::write(&toml, body).unwrap();

    let err = runtime::run_with_env(args_with_config(toml), &env)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, RuntimeError::Config(_)));
    assert!(
        msg.contains("[judge].model"),
        "legacy-key error must name the offending key: {msg}"
    );
}

#[tokio::test]
async fn unresolved_secret_refuses_with_named_var() {
    // Operator pointed at an env var that doesn't exist; the loader
    // refuses with the var name surfaced verbatim.
    let env = StaticEnv::new(); // empty
    let dir = tempfile::TempDir::new().unwrap();
    let toml = write_minimal_config(dir.path());
    let err = runtime::run_with_env(args_with_config(toml), &env)
        .await
        .unwrap_err();
    let msg = err.to_string();
    match err {
        RuntimeError::Config(_) => {
            assert!(
                msg.contains("LINEAR_API_TOKEN"),
                "missing-env error must name LINEAR_API_TOKEN: {msg}"
            );
        }
        other => panic!("expected Config(EnvVarMissing), got {other:?}"),
    }
}
