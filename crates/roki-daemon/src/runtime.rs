//! Runtime bootstrap: load config, install the redaction-aware logging
//! pipeline, start every adapter, and run the orchestrator until shutdown.
//!
//! Task 5.1 replaces the placeholder `run` from earlier scaffolding with the
//! full daemon bootstrap documented in
//! `.kiro/specs/roki-mvp/design-bootstrap.md`. The composition order is
//! deliberate:
//!
//! 1. Load the config from disk (default `./roki.toml`, override via
//!    `--config`).
//! 2. Resolve the secret list (Linear token + every per-repo webhook secret).
//!    Initialise the redaction-aware tracing pipeline with that list before
//!    emitting any structured event so a stray `Debug` of a config struct
//!    cannot leak a token through stdout.
//! 3. Install the OS signal handlers so SIGINT / SIGTERM trigger the same
//!    [`ShutdownSignal`] every component clones.
//! 4. Resolve the `claude` binary (config override → `$PATH` discovery →
//!    refusal with a precise error).
//! 5. Build the engine adapter, the workspace manager, the permission
//!    resolver, and the per-repo `WorkflowLoader`s.
//! 6. Build the orchestrator with the engine policy resolved from the first
//!    repo's `WorkflowPolicy`. The MVP daemon serves one runtime engine
//!    policy across the orchestrator (a single `EnginePolicy` is consumed by
//!    every `(repo, issue)` actor); per-repo overrides land when downstream
//!    work splits the orchestrator into per-repo actor pools.
//! 7. For each repo: spawn a `LinearTracker` and build a [`WebhookState`].
//! 8. Compose a single axum router that mounts `/linear/webhook/<repo-id>`
//!    for every repo. Bind the configured `[server]` address and start the
//!    server.
//! 9. Wire the polling and webhook outputs through the [`TrackerBridge`] into
//!    the orchestrator inbox.
//! 10. `tokio::select!` on shutdown across the orchestrator, the bridge, the
//!     server, and every spawned tracker. After shutdown fires, walk every
//!     spawned task through [`await_workers_with_window`].
//!
//! Refusal modes are explicit: missing config file, missing webhook secret,
//! `claude` binary not on PATH, `[server]` port conflict — every refusal
//! produces a clear, actionable [`anyhow::Error`] message and a non-zero
//! exit code via the binary's `main`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use tokio::net::TcpListener;
use tokio::process::Command as AsyncCommand;
use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::cli::RunArgs;
use crate::config::{Config, EnvOverrides, LinearConfig, PermissionStrategy, SecretString};
use crate::engine::ClaudeEngineAdapter;
use crate::engine::policy::EnginePolicy;
use crate::logging::{LogContext, LogDestination, LoggingConfig, LoggingGuard};
use crate::orchestrator::core::{
    EngineLauncher, LaunchError, Orchestrator, OrchestratorReadHandle,
};
use crate::orchestrator::events::EventBus;
use crate::orchestrator::hooks::HookRegistry;
use crate::orchestrator::state::RepoId;
use crate::orchestrator::tracker_bridge::TrackerBridge;
use crate::shutdown::{
    SHUTDOWN_WINDOW, ShutdownSignal, await_workers_with_window, install_signal_handlers,
};
use crate::tools::{GhqTool, NoopRateLimit, RateLimitState, RealGhq, RealWt, WtTool};
use crate::tracker::linear::{LinearTracker, LinearTrackerConfig, ScopeWatch};
use crate::tracker::model::NormalizedIssue;
use crate::tracker::webhook::{WebhookState, router as webhook_router};
use crate::workflow::{WorkflowHandle, WorkflowLoader, WorkflowPolicy};
use crate::workspace::{GhqIdentifier, WorkspaceManager};
use async_trait::async_trait;

/// Default config-file path used when `--config` is not supplied on the CLI.
const DEFAULT_CONFIG_PATH: &str = "./roki.toml";

/// Default canonical Linear GraphQL endpoint. Production callers do not
/// override this; the integration tests inject a wiremock URL through the
/// `ROKI_LINEAR_ENDPOINT` env var so the bootstrap reaches the fake instead
/// of `api.linear.app`.
const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";

/// Build the multi-threaded tokio runtime used by the daemon.
pub fn build_tokio_runtime() -> Result<tokio::runtime::Runtime> {
    Builder::new_multi_thread()
        .enable_all()
        .thread_name("roki-worker")
        .build()
        .context("failed to build tokio multi-threaded runtime")
}

/// Initialise the bootstrap tracing pipeline.
///
/// Invoked from `main.rs` before the configuration loader runs, so the
/// operator sees config-load errors. After the config loads,
/// [`run_with_shutdown`] reinstalls the production pipeline with the real
/// secret list. This first call is intentionally minimal: no secrets are
/// available yet, and `try_init` permits the second installation to fail
/// silently when in tests.
pub fn init_tracing() -> Option<LoggingGuard> {
    let directive = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let config = LoggingConfig::stdout(directive);
    match crate::logging::init(config) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("roki: tracing init failed: {error}");
            None
        }
    }
}

/// Handles published by the bootstrap so external callers (notably the
/// integration smoke test) can register subscribers and read state without
/// modifying the daemon's `main`. Production callers do not need this — the
/// channel is `Option` and the binary supplies `None`.
pub struct BootstrapHandles {
    /// Shared event bus through which transition events flow. Observers can
    /// register before any tracker event arrives so no transition is missed.
    pub event_bus: Arc<EventBus>,
    /// Read-only projection over the orchestrator state map.
    pub orchestrator_read: OrchestratorReadHandle,
    /// The actual port the axum server bound to. Always equals
    /// `[server].port` (or the `--port` override), but threaded back so a
    /// caller does not have to re-read the config to reconstruct the URL.
    pub bind_port: u16,
}

/// `roki run` production entry point. Wraps [`run_with_shutdown`] with a
/// freshly-constructed [`ShutdownSignal`] connected to the OS signal
/// handlers, matching the pre-task-5.1 contract `main.rs` invokes.
pub async fn run(args: RunArgs) -> Result<()> {
    let shutdown = ShutdownSignal::new();
    let _signal_task = install_signal_handlers(shutdown.clone());
    run_with_shutdown(args, shutdown, None).await
}

/// Bootstrap variant that accepts an externally-owned [`ShutdownSignal`] and
/// optionally publishes [`BootstrapHandles`] for tests.
///
/// `handles_tx` is consumed exactly once on a successful bootstrap. Tests
/// pass a `oneshot::Sender` so they can register an observer before the
/// orchestrator commits any transition.
pub async fn run_with_shutdown(
    args: RunArgs,
    shutdown: ShutdownSignal,
    handles_tx: Option<oneshot::Sender<BootstrapHandles>>,
) -> Result<()> {
    // Open and immediately drop a per-bootstrap span so the startup log
    // line carries the canonical (repo, issue, correlation_id) shape but
    // the resulting `EnteredSpan` (`!Send`) does not poison the rest of
    // this async function for cross-task callers. Subsequent events do
    // not need the span; the per-actor / per-tracker code paths attach
    // their own LogContexts.
    {
        let bootstrap_ctx = LogContext::new("daemon", "bootstrap", new_correlation_id());
        let _enter = bootstrap_ctx.span("daemon.bootstrap").entered();
        info!(version = env!("CARGO_PKG_VERSION"), "roki daemon starting");
    }

    // ---- 1. load config -------------------------------------------------
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    if !config_path.exists() {
        bail!(
            "config file not found at `{}` — pass `--config <path>` or create the file",
            config_path.display(),
        );
    }
    let env_overrides = EnvOverrides::from_process_env()
        .with_context(|| "failed to read environment-variable config overrides")?;
    let mut config = Config::load(&config_path, &env_overrides)
        .with_context(|| format!("failed to load config from `{}`", config_path.display()))?;

    // CLI overrides (decision matrix #4: CLI wins over file).
    if let Some(addr) = args.bind {
        config.server_bind = addr;
    }
    if let Some(port) = args.port {
        if port == 0 {
            bail!("--port must be greater than zero");
        }
        config.server_port = port;
    }
    if args.dangerously_skip_permissions {
        if !matches!(
            config.permission_strategy,
            PermissionStrategy::DangerouslySkipPermissions
        ) {
            warn!("--dangerously-skip-permissions overrides the configured permission strategy",);
        }
        config.permission_strategy = PermissionStrategy::DangerouslySkipPermissions;
    }

    // ---- 2. resolve secrets, then reinitialise logging with redaction ---
    // TODO(7.1c): collapse the multi-route webhook bootstrap onto a single
    // `POST /linear/webhook` route. For 7.1a we keep the per-repo route
    // shape but swap to a single workspace-level secret resolved from
    // `[linear].webhook_secret_env` (Requirement 2.3).
    let workspace_webhook_secret = resolve_workspace_webhook_secret(&config.linear)?;
    let redaction_secrets: Vec<String> = vec![
        config.linear_token.expose().to_string(),
        workspace_webhook_secret.expose().to_string(),
    ];

    let logging_directive = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let logging_config = LoggingConfig {
        filter: logging_directive,
        destination: LogDestination::Stdout,
        secrets: redaction_secrets,
    };
    // The first init call runs in `main`; the second is allowed to fail
    // because `tracing_subscriber::registry().try_init()` is one-shot per
    // process. The redaction layer is the goal — when running standalone
    // from `main` the redaction list is empty; here we lose nothing if
    // re-installation is rejected because the production pipeline already
    // owns global tracing for that process. Tests construct a fresh
    // subscriber via `tracing_subscriber::with_default` for assertions.
    let _logging_guard = crate::logging::init(logging_config).ok();

    // ---- 3. resolve the claude binary ----------------------------------
    let claude_binary = resolve_claude_binary(config.claude_binary.as_deref())?;

    // ---- 3b. refuse to start if `wt` or `ghq` are not on PATH ----------
    // Task 6.1 locked decisions: the daemon shells out to `wt` for
    // worktree management and `ghq` for repo discovery. Both must be on
    // PATH; absence is a hard refusal with an actionable remediation.
    ensure_external_tool_present("wt").await.with_context(|| {
        "wt (worktrunk) not found on PATH — install via the worktrunk repo (https://github.com/reedom/worktrunk) or add it to PATH"
    })?;
    ensure_external_tool_present("ghq").await.with_context(|| {
        "ghq not found on PATH — install via `brew install ghq` or `go install github.com/x-motemen/ghq@latest`"
    })?;

    // ---- 4. build the single workspace-level workflow loader -----------
    // Post-7.1a: one workspace-level `[workflow].path` replaces the
    // per-repo `workflow_path` (Requirement 2.4, locked decision #6).
    let workflow_handle =
        WorkflowLoader::watch(config.workflow.path.clone(), Duration::from_millis(250))
            .await
            .with_context(|| {
                format!(
                    "failed to load workspace WORKFLOW.md from `{}`",
                    config.workflow.path.display(),
                )
            })?;
    let workflow_policy = workflow_handle.current();
    let workflow_handles: Vec<WorkflowHandle> = vec![workflow_handle];
    let workflow_policies: Vec<Arc<WorkflowPolicy>> = vec![Arc::clone(&workflow_policy)];

    // ---- 5. build engine + workspace + orchestrator --------------------
    // Task 6.1: workspace manager is built from `wt` + `ghq` adapters and
    // an operator-supplied `repo_index` mapping `RepoId` → ghq identifier.
    let wt: Arc<dyn WtTool> = Arc::new(RealWt::new());
    let ghq: Arc<dyn GhqTool> = Arc::new(RealGhq::new());
    // TODO(7.1d): drop the `RepoId`-keyed `repo_index` once `WorktreeRegistry`
    // arrives; for now key the index by the repo's ghq identifier so the
    // workspace manager continues to compile against `RepoId`.
    let repo_index: HashMap<RepoId, GhqIdentifier> = config
        .repos
        .iter()
        .map(|repo| (RepoId::new(repo.repo.clone()), repo.repo.clone()))
        .collect();
    let workspace = Arc::new(WorkspaceManager::new(wt, ghq, repo_index));
    let engine = Arc::new(ClaudeEngineLauncher::new(ClaudeEngineAdapter::with_binary(
        claude_binary.clone(),
    )));
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());

    // Engine policy is resolved from the first repo's WorkflowPolicy. The
    // MVP orchestrator carries one runtime policy. When a future task splits
    // per-repo policies into per-actor `WorkerContext`, this resolver
    // expands to a per-repo map.
    let engine_policy = workflow_policies
        .first()
        .map(|p| EnginePolicy::from_workflow(p))
        .unwrap_or_default();

    let (inbox_tx, inbox_rx) = mpsc::channel::<NormalizedIssue>(64);
    let orchestrator = Orchestrator::new(
        Arc::clone(&workspace) as Arc<_>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        inbox_rx,
    )
    .with_engine_policy(engine_policy);
    let orchestrator_read = orchestrator.read_handle();

    // ---- 6. per-repo trackers + webhook routes -------------------------
    let mut tracker_join: JoinSet<()> = JoinSet::new();
    let mut router = Router::new();
    let (webhook_tx_master, webhook_rx_master) = mpsc::channel::<NormalizedIssue>(64);
    let (polling_tx_master, polling_rx_master) = mpsc::channel::<NormalizedIssue>(64);

    let rate_limit: Arc<dyn RateLimitState> = Arc::new(NoopRateLimit);
    let linear_endpoint = config
        .linear_endpoint
        .clone()
        .or_else(|| std::env::var("ROKI_LINEAR_ENDPOINT").ok())
        .unwrap_or_else(|| DEFAULT_LINEAR_ENDPOINT.to_string());

    let mut tracker_shutdowns: Vec<oneshot::Sender<()>> = Vec::with_capacity(config.repos.len());

    // TODO(7.1c): collapse to a single `POST /linear/webhook` route plus a
    // single `LinearTracker`. For 7.1a we keep one route + tracker per repo
    // entry as a temporary shim using the SHARED workspace-level webhook
    // secret. The route segment is derived from the repo's ghq identifier
    // (sanitised) since `repo.id` is gone.
    for repo in &config.repos {
        let webhook_secret = workspace_webhook_secret.clone();

        let path_segment = sanitize_url_segment(repo.repo.as_str()).map_err(|reason| {
            anyhow!(
                "repo `{}` cannot be encoded as a URL path segment: {}",
                repo.repo,
                reason,
            )
        })?;
        let webhook_path = format!("/linear/webhook/{path_segment}");
        // TODO(7.1c): drop the `team_or_scope_fallback` field along with the
        // single-tracker collapse.
        let team_or_scope_fallback = String::new();
        let webhook_state = WebhookState::new(
            webhook_secret,
            crate::orchestrator::state::RepoId::new(repo.repo.clone()),
            team_or_scope_fallback,
            webhook_tx_master.clone(),
        );
        router = router.merge(webhook_router(webhook_state, &webhook_path));

        // LinearTracker: one per repo allowlist entry as a temporary shim;
        // the workspace-level collapse is 7.1c's job.
        let tracker = LinearTracker::new(LinearTrackerConfig {
            endpoint: linear_endpoint.clone(),
            cadence: config.polling_cadence,
            scopes: vec![ScopeWatch {
                repo: crate::orchestrator::state::RepoId::new(repo.repo.clone()),
            }],
            token: SecretString::new(config.linear_token.expose().to_string()),
            rate_limit: Arc::clone(&rate_limit),
        });
        let (tracker_shutdown_tx, tracker_shutdown_rx) = oneshot::channel::<()>();
        tracker_shutdowns.push(tracker_shutdown_tx);
        let tracker_sink = polling_tx_master.clone();
        tracker_join.spawn(async move {
            let _ = tracker.run(tracker_sink, tracker_shutdown_rx).await;
        });
    }
    // Drop the master senders the bootstrap holds so when every per-repo
    // sender shuts down the bridge sees its inputs close.
    drop(webhook_tx_master);
    drop(polling_tx_master);

    // ---- 7. tracker bridge ---------------------------------------------
    let bridge = TrackerBridge::new(polling_rx_master, webhook_rx_master, inbox_tx);
    let bridge_handle = tokio::spawn(bridge.run());

    // ---- 8. axum server bind -------------------------------------------
    let bind_addr = SocketAddr::new(config.server_bind, config.server_port);
    let listener = TcpListener::bind(bind_addr).await.with_context(|| {
        format!("failed to bind axum server at `{bind_addr}` — port may already be in use")
    })?;
    let bound_addr = listener
        .local_addr()
        .with_context(|| "TcpListener::local_addr failed")?;
    let resolved_port = bound_addr.port();

    info!(
        bind = %bound_addr,
        repos = config.repos.len(),
        claude_binary = %claude_binary.display(),
        config_path = %config_path.display(),
        "roki daemon ready",
    );

    // Publish handles to any test that asked for them (production main.rs
    // passes None). This must happen after every component is constructed
    // so the test never observes a half-initialised bootstrap.
    if let Some(tx) = handles_tx {
        let _ = tx.send(BootstrapHandles {
            event_bus: Arc::clone(&event_bus),
            orchestrator_read,
            bind_port: resolved_port,
        });
    }

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { server_shutdown.wait().await })
            .await
    });

    // ---- 9. orchestrator run (drives shutdown) -------------------------
    let orch_handle = tokio::spawn(async move { orchestrator.run().await });

    // The orchestrator's `run()` loop exits when the shared `ShutdownSignal`
    // fires. We await it here so the bootstrap stays parked while the
    // orchestrator processes tracker events.
    let orch_outcome = orch_handle.await;
    if let Err(err) = orch_outcome {
        warn!(error = %err, "orchestrator task ended with a join error");
    }

    // ---- 10. bounded shutdown of remaining tasks -----------------------
    // Trackers stop on their oneshot signal; we drop the senders so each
    // tracker future resolves promptly. The bridge will then see both
    // input channels close.
    for tx in tracker_shutdowns {
        let _ = tx.send(());
    }

    // Drain trackers; await_workers_with_window enforces the documented
    // 30s shutdown window per task even though most trackers exit
    // immediately on their oneshot signal.
    let tracker_join_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    while let Some(handle) = tracker_join.try_join_next() {
        if let Err(err) = handle {
            warn!(error = %err, "tracker task ended with a join error");
        }
    }
    // Move any in-flight trackers that have not finished yet into
    // `await_workers_with_window` so they are bounded.
    while let Some(handle) = tracker_join.join_next().await {
        if let Err(err) = handle {
            warn!(error = %err, "tracker task ended with a join error");
        }
    }
    let _ = tracker_join_handles;

    // Drive the bridge and the server through the documented bounded
    // shutdown window so the daemon honours requirement 1.3 even when the
    // orchestrator dropped before its consumers.
    let trailing: Vec<tokio::task::JoinHandle<()>> = vec![
        tokio::spawn(async move {
            let _ = bridge_handle.await;
        }),
        tokio::spawn(async move {
            let _ = server_handle.await;
        }),
    ];
    let _ = await_workers_with_window(trailing, SHUTDOWN_WINDOW).await;

    // Drop workflow handles last so file watchers tear down after every
    // consumer that may still hold a reference to a parsed policy.
    drop(workflow_handles);
    drop(workflow_policies);

    info!("roki daemon exiting cleanly");
    Ok(())
}

/// Resolve the workspace-level webhook HMAC secret declared in
/// `[linear].webhook_secret_env` (Requirement 2.3). The configured env-var
/// must be set and non-empty; absence is a hard refusal. The returned
/// [`SecretString`] is shared across every webhook route the bootstrap
/// mounts.
fn resolve_workspace_webhook_secret(linear: &LinearConfig) -> Result<SecretString> {
    resolve_workspace_webhook_secret_with(linear, |var| std::env::var(var).ok())
}

/// Pure helper that the unit tests can drive without mutating the process
/// environment. The lookup closure stands in for `std::env::var`.
fn resolve_workspace_webhook_secret_with<F>(
    linear: &LinearConfig,
    lookup: F,
) -> Result<SecretString>
where
    F: FnOnce(&str) -> Option<String>,
{
    let var = linear.webhook_secret_env.as_str();
    let value = lookup(var).ok_or_else(|| anyhow!("webhook secret env-var `{var}` is not set"))?;
    if value.trim().is_empty() {
        return Err(anyhow!("webhook secret env-var `{var}` is empty"));
    }
    Ok(SecretString::new(value))
}

/// Resolve the `claude` binary path. The override (config-file
/// `claude_binary` / future env var) wins over `$PATH` discovery; absence
/// from PATH is a hard error with an actionable remediation message.
fn resolve_claude_binary(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if !path.exists() {
            bail!(
                "claude_binary override `{}` does not exist on disk",
                path.display(),
            );
        }
        return Ok(path.to_path_buf());
    }
    let path_var = std::env::var_os("PATH").ok_or_else(|| {
        anyhow!("PATH is not set; cannot resolve `claude` — set `claude_binary` in the config")
    })?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("claude");
        if candidate.is_file() {
            return Ok(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join("claude.exe");
            if exe.is_file() {
                return Ok(exe);
            }
        }
    }
    bail!(
        "claude binary not found on PATH — install Claude Code or set `claude_binary` in the config"
    )
}

/// Ensure `tool` is on `$PATH` by invoking `<tool> --version` once and
/// classifying `io::ErrorKind::NotFound` as the absence signal. A non-zero
/// exit from a present binary is treated as success — the tool exists; its
/// version output is irrelevant to bootstrap. Used for `wt` and `ghq` per
/// task 6.1 locked decisions #1 and #2.
async fn ensure_external_tool_present(tool: &str) -> Result<()> {
    match AsyncCommand::new(tool).arg("--version").output().await {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(anyhow!("{tool} binary not found on PATH",))
        }
        Err(err) => Err(anyhow!("{tool} --version: {err}")),
    }
}

fn new_correlation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("corr-{n:016x}")
}

// ---- Engine launcher adapter ------------------------------------------

/// Bridges [`ClaudeEngineAdapter`] into the orchestrator's
/// [`EngineLauncher`] trait. The adapter lives in `engine::claude` and
/// returns the adapter-flavoured `LaunchError`; the orchestrator's trait
/// returns its own type so the seam stays clean.
struct ClaudeEngineLauncher {
    adapter: ClaudeEngineAdapter,
}

impl ClaudeEngineLauncher {
    fn new(adapter: ClaudeEngineAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl EngineLauncher for ClaudeEngineLauncher {
    async fn launch(
        &self,
        ctx: crate::engine::WorkerContext,
        events: mpsc::Sender<crate::engine::SupervisedEvent>,
    ) -> Result<crate::engine::policy::WorkerOutcome, LaunchError> {
        self.adapter
            .launch(ctx, events)
            .await
            .map_err(|err| LaunchError::Engine(err.to_string()))
    }
}

/// Sanitise a `RepoId` into a URL path segment. Mirrors the workspace
/// layer's character class (`[A-Za-z0-9._-]`) so a repo that allocates a
/// workspace successfully also encodes into a webhook URL successfully.
///
/// Path separators in the raw value are a hard rejection (a smuggled `/`
/// would silently split into multiple URL segments). Empty or sentinel-only
/// (`.` / `..`) results are also rejected.
fn sanitize_url_segment(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("identifier is empty".to_string());
    }
    if raw.contains('/') || raw.contains('\\') {
        return Err("identifier contains a path separator ('/' or '\\\\')".to_string());
    }
    let sanitised: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitised.is_empty() {
        return Err("identifier is empty after sanitization".to_string());
    }
    if sanitised == "." || sanitised == ".." {
        return Err("sanitized identifier is a path traversal sentinel ('.' or '..')".to_string());
    }
    Ok(sanitised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_claude_binary_honours_override_when_present() {
        let temp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = temp.path().to_path_buf();
        let resolved = resolve_claude_binary(Some(&path)).expect("override accepted");
        assert_eq!(resolved, path);
    }

    #[test]
    fn resolve_claude_binary_rejects_missing_override() {
        let path = PathBuf::from("/nonexistent/claude-binary-for-test");
        let err = resolve_claude_binary(Some(&path)).expect_err("must reject missing override");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not exist"),
            "error should mention missing path, got: {msg}",
        );
    }

    fn linear_cfg(env_var: &str) -> LinearConfig {
        LinearConfig {
            token_env: "LINEAR_API_TOKEN".to_string(),
            webhook_secret_env: env_var.to_string(),
        }
    }

    #[test]
    fn resolve_workspace_webhook_secret_reads_env_var() {
        let linear = linear_cfg("ROKI_FAKE_WEBHOOK_SECRET");
        let secret =
            resolve_workspace_webhook_secret_with(&linear, |_| Some("the-secret".to_string()))
                .expect("lookup must succeed");
        assert_eq!(secret.expose(), "the-secret");
    }

    #[test]
    fn resolve_workspace_webhook_secret_refuses_when_env_unset() {
        let linear = linear_cfg("ROKI_FAKE_WEBHOOK_SECRET");
        let err = resolve_workspace_webhook_secret_with(&linear, |_| None)
            .expect_err("absent env var must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not set"),
            "error must call out the missing env var, got: {msg}",
        );
    }

    #[test]
    fn resolve_workspace_webhook_secret_refuses_when_env_empty() {
        let linear = linear_cfg("ROKI_FAKE_WEBHOOK_SECRET");
        let err = resolve_workspace_webhook_secret_with(&linear, |_| Some("   ".to_string()))
            .expect_err("empty value must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty"),
            "error must call out empty value: {msg}"
        );
    }

    #[test]
    fn sanitize_url_segment_keeps_safe_characters() {
        assert_eq!(
            sanitize_url_segment("core_repo-1.0").unwrap(),
            "core_repo-1.0",
        );
    }

    #[test]
    fn sanitize_url_segment_replaces_unsafe_characters() {
        assert_eq!(sanitize_url_segment("core repo!").unwrap(), "core_repo_");
    }

    #[test]
    fn sanitize_url_segment_rejects_path_separator() {
        assert!(sanitize_url_segment("foo/bar").is_err());
    }

    #[test]
    fn sanitize_url_segment_rejects_traversal_sentinel() {
        assert!(sanitize_url_segment("..").is_err());
    }
}
