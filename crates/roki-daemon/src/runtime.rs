//! Daemon bootstrap composition (`run_with_shutdown`).
//!
//! This module wires the canonical 12-step composition order documented in
//! design.md "Daemon bootstrap" so the binary entry point reduces to a single
//! `runtime::run(args).await` call.
//!
//! The implementation surface is split between this module (orchestration of
//! the steps) and the existing subsystem modules (config, logging, shutdown,
//! tracker, workflow, engine, orchestrator) which already own their step-local
//! validation. Composition here exists to:
//!
//! - Order the steps deterministically so refusals at any step name the
//!   offending field/path/binary.
//! - Apply CLI overrides from [`crate::cli::RunArgs`] over the loaded config.
//! - Refuse non-zero on missing required binaries (`wt`, `ghq`, `claude`),
//!   port conflicts, legacy config keys, or unreachable secrets.
//! - Mount `POST /linear/webhook` on a single workspace-level
//!   [`tracker::webhook::WebhookState`] and wind down within
//!   [`crate::shutdown::SHUTDOWN_WINDOW`].
//!
//! Tasks 10.1, 10.2, 10.3 share the bootstrap surface; the missing-binary and
//! port-conflict refusals are step-local checks, exercised by integration
//! tests in `tests/runtime_smoke.rs` and `tests/runtime_bind.rs`. The full
//! e2e_bootstrap (Task 13.1) lives separately so this module's tests focus on
//! the refusal paths and composition shape.
//!
//! Spec refs: requirements.md Req 1.1, 1.3, 2.5, 2.12, 7.1, 7.2, 7.3.

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use thiserror::Error;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::cli::RunArgs;
use crate::config::{
    AssigneeSpec, Config, ConfigError, EnvReader, PermissionStrategy as ConfigPermissionStrategy,
    ProcessEnv, SecretValue,
};
use crate::engine::claude::{ClaudeBinary, ClaudeError};
use crate::engine::orchestrator_session::adapter::OrchestratorSessionAdapter;
use crate::engine::orchestrator_session::engine_impl::OrchestratorEngineImpl;
use crate::engine::phase_subprocess::adapter::PhaseSubprocessAdapter;
use crate::engine::phase_subprocess::engine_impl::PendingPhaseEngine;
use crate::exec::ghq::RealGhq;
use crate::exec::wt::RealWt;
use crate::orchestrator::core::{
    ActorMessage, Orchestrator, OrchestratorDeps, OrchestratorEngine, PhaseEngine, SessionDirOps,
    WorktreeOps,
};
use crate::orchestrator::escalation::EscalationQueue;
use crate::orchestrator::events::EventBus;
use crate::orchestrator::hooks::SubscriberHooks;
use crate::orchestrator::read::{ActorSnapshot, OrchestratorReadHandle};
use crate::orchestrator::state::IssueId;
use crate::permissions::{PermissionConfigError, PermissionResolver};
use crate::session::SessionManager;
use crate::shutdown::{
    self, SHUTDOWN_WINDOW, ShutdownSignal, await_workers_with_window, install_signal_handlers,
};
use crate::tracker::model::NormalizedIssue;
use crate::tracker::webhook::{WebhookState, router as webhook_router};
use crate::workflow::schema::WorkflowPolicy;
use crate::workflow::watcher::{WatcherError, load_policy};
use crate::worktree_manager::WorktreeManager;

/// Errors surfaced by [`run`]. Each variant carries the step-local context the
/// operator needs to fix the misconfiguration without consulting docs.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error(
        "configuration file `{path}` not found; pass `--config <path>` or place \
         a `roki.toml` in the working directory"
    )]
    ConfigFileMissing { path: PathBuf },

    #[error("`claude` binary not usable: {0}")]
    ClaudeBinary(#[from] ClaudeError),

    #[error(
        "required executable `{name}` was not found on PATH; install it (or set \
         PATH) before starting roki — see docs/reference/cli.md for the full \
         prerequisite list"
    )]
    ExternalBinaryMissing { name: &'static str },

    #[error(
        "could not bind webhook server on `{addr}`: {source}; another process is \
         likely listening — set `[server].port` or pass `--port`"
    )]
    BindFailed {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("logging initialization failed: {0}")]
    Logging(#[from] crate::logging::LoggingError),

    #[error(
        "Linear assignee selector `{selector}` could not be resolved: explicit \
         selectors require a user-directory lookup which the bootstrap layer \
         does not yet implement; configure `[linear].assignee = \"me\"` or wait \
         for the user-directory resolver to land"
    )]
    AssigneeResolve { selector: String },

    #[error("failed to load workflow policy from `{path}`: {source}")]
    WorkflowLoad {
        path: PathBuf,
        #[source]
        source: WatcherError,
    },

    /// Composite refusal surfaced when an engine adapter or manager factory
    /// returns an error during runtime composition. The `component` field
    /// names the offending construction step so the operator log entry points
    /// at exactly one factory.
    #[error("failed to construct runtime component `{component}`: {message}")]
    ComponentAssembly {
        component: &'static str,
        message: String,
    },
}

/// Top-level entry point dispatched by `main.rs`. Drives the documented
/// 12-step bootstrap composition.
pub async fn run(args: RunArgs) -> Result<(), RuntimeError> {
    let env = ProcessEnv;
    run_with_env(args, &env).await
}

/// Test-friendly variant that accepts an injected [`EnvReader`].
pub async fn run_with_env(args: RunArgs, env: &dyn EnvReader) -> Result<(), RuntimeError> {
    let bootstrap = bootstrap(args, env).await?;
    serve(bootstrap).await
}

/// Result of a successful bootstrap: every refusal has cleared, the webhook
/// listener is bound, and the shutdown signal is wired. The caller drives the
/// `serve` step which awaits shutdown and winds down within `SHUTDOWN_WINDOW`.
struct Bootstrapped {
    shutdown_signal: ShutdownSignal,
    /// Held so the listener stays bound; `serve` consumes it.
    listener: TokioTcpListener,
    bind_addr: String,
    webhook_state: Arc<WebhookState>,
    issue_rx: mpsc::Receiver<NormalizedIssue>,
    _logging_guard: crate::logging::LoggingGuard,
    _signal_task: tokio::task::JoinHandle<()>,
    /// Engine adapters + managers assembled from the loaded `WorkflowPolicy`,
    /// resolved external binaries, and config-derived permission strategy.
    /// Held so subsequent composition steps (Tasks 10.1.2-10.1.6) consume a
    /// single anchor.
    #[allow(dead_code)]
    components: RuntimeComponents,
    /// Per-issue actor-map container assembled in step 8 (design.md "Daemon
    /// bootstrap"). Seeded empty in 10.1.2; recovery seed lands in 10.1.3.
    #[allow(dead_code)]
    orchestrator: Arc<Orchestrator>,
    /// Inbox handle bridging the tracker / admission pipe (10.1.4-10.1.5)
    /// to the per-issue actors. Held so step-11 wiring can clone it.
    #[allow(dead_code)]
    inbox: OrchestratorInbox,
    /// Read-only projection over the per-issue state map + escalation
    /// queue. Consumed by TUI/JSON snapshot endpoints (Task 13.x).
    #[allow(dead_code)]
    read_handle: Arc<OrchestratorReadHandle>,
    /// Held so escalation snapshots survive the bootstrap return.
    #[allow(dead_code)]
    escalations: Arc<EscalationQueue>,
}

/// Assembled engine adapters + managers handed to subsequent composition
/// steps in `run_with_shutdown`. Construction is gated by
/// [`assemble_runtime_components`]; any factory error is converted into
/// [`RuntimeError::ComponentAssembly`] naming the offending component so the
/// operator log entry on refusal points at exactly one factory.
///
/// Future tasks (10.1.2-10.1.6) extend this struct with additional fields
/// (orchestrator inbox, recovery handle, tracker handle, shutdown wiring).
/// The current shape is intentionally additive.
// Fields are consumed by future composition steps (Tasks 10.1.2-10.1.6); the
// dead-code lint is silenced rather than dropping fields the next task needs.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct RuntimeComponents {
    /// Resolved `claude` binary path (config override > PATH search).
    pub claude_binary: ClaudeBinary,
    /// Resolved `wt` external binary path.
    pub wt_path: PathBuf,
    /// Resolved `ghq` external binary path.
    pub ghq_path: PathBuf,
    /// Loaded + validated workflow policy.
    pub workflow_policy: Arc<WorkflowPolicy>,
    /// Per-issue session tempdir manager.
    pub session_manager: Arc<SessionManager>,
    /// Per-issue worktree lifecycle manager (driven by the resolved `wt` /
    /// `ghq` adapters and the `[[repos]]` allowlist).
    pub worktree_manager: Arc<WorktreeManager<RealWt, RealGhq>>,
    /// Operator-controlled phase-subprocess permission resolver. Already
    /// gated through [`PermissionResolver::ensure_phase_strategy_present`];
    /// holders may invoke `resolve_for_phase` without re-checking.
    pub permission_resolver: Arc<PermissionResolver>,
    /// Long-lived orchestrator session adapter (one per ticket at runtime).
    pub orchestrator_session_adapter: Arc<OrchestratorSessionAdapter>,
    /// Bounded phase-subprocess adapter (one launch per `action=run_phase`).
    pub phase_subprocess_adapter: Arc<PhaseSubprocessAdapter>,
}

/// Step 1-11 of the daemon bootstrap. Step 12 (the `tokio::select!` wind-down)
/// is the caller's responsibility — production routes through [`serve`];
/// tests skip the await loop and call [`Bootstrapped`] field-by-field.
async fn bootstrap(args: RunArgs, env: &dyn EnvReader) -> Result<Bootstrapped, RuntimeError> {
    // ---- Step 1: load config (CLI override > file > documented defaults).
    let config_path = resolve_config_path(args.config.as_deref())?;
    let mut config = Config::load_from_path(&config_path)?;
    apply_cli_overrides(&mut config, &args);

    // ---- Step 2: resolve secrets via the injected reader.
    let api_token = config.linear.api_token.resolve(env)?;
    let webhook_secret = config.linear.webhook_secret.resolve(env)?;

    // ---- Step 3: redaction-aware logging init.
    let logging_guard = init_logging_with_redaction(&config, &api_token, &webhook_secret)?;

    // ---- Step 4: assignee resolution. `me` is resolved later (requires a
    // network call); explicit selectors must be flagged here as unsupported
    // by the bootstrap layer per Task 3.4 — the resolver runtime owns the
    // user-directory lookup. We refuse early so the operator gets one log
    // line naming the offending key rather than a tracker startup failure.
    if let AssigneeSpec::Selector(value) = &config.linear.assignee {
        return Err(RuntimeError::AssigneeResolve {
            selector: value.clone(),
        });
    }

    // ---- Step 5: admit_states (already non-empty post-load; the loader
    // refuses an empty resolved set per Req 2.10).

    // ---- Step 6: install signal handlers wired to a shared ShutdownSignal.
    let (shutdown_signal, shutdown_trigger) = shutdown::new();
    // The signal handler task owns the trigger — when SIGINT/SIGTERM fires,
    // every `ShutdownSignal::wait()` subscriber wakes.
    let signal_task = install_signal_handlers(shutdown_trigger);

    // ---- Step 7: claude binary discovery; refuse on missing `wt`/`ghq`.
    let claude_binary = ClaudeBinary::discover(None)?;
    let wt_path = resolve_external_binary("wt")?;
    let ghq_path = resolve_external_binary("ghq")?;

    // ---- Step 8: workflow load + engine adapter / manager assembly. Loads,
    // parses, and validates `WORKFLOW.md` (the readability-only check is
    // subsumed by the full load below) before constructing the
    // `RuntimeComponents` anchor consumed by Tasks 10.1.2-10.1.6.
    config.validate_workflow_readable()?;
    let workflow_policy =
        load_policy(&config.workflow.path)
            .await
            .map_err(|source| RuntimeError::WorkflowLoad {
                path: config.workflow.path.clone(),
                source,
            })?;

    let permission_resolver = build_permission_resolver(&config);
    let components = assemble_runtime_components(RuntimeAssemblyInputs {
        claude_binary: claude_binary.clone(),
        wt_path: wt_path.clone(),
        ghq_path: ghq_path.clone(),
        workflow_policy,
        repos_allowlist: config.repos.clone(),
        permission_resolver,
    })?;

    // ---- Step 8 (continued): compose the orchestrator actor map around
    // the assembled `RuntimeComponents`. Seeded empty per 10.1.2 — recovery
    // wiring lands in 10.1.3. The phase pipeline is wired by 10.1.5; until
    // then `PendingPhaseEngine` refuses every `run_phase` so a misrouted
    // phase nomination surfaces a structured error rather than a silent
    // miswiring.
    let orchestrator_seams = ProductionOrchestratorSeams::from_components(&components);
    let composition = compose_orchestrator(orchestrator_seams);

    // ---- Step 9-10: tracker construction is deferred to Task 10.1.4. The
    // webhook bind is the load-bearing refusal point under Tasks 10.1-10.3.

    // ---- Step 11: bind webhook listener. Hard-refuse on port conflict.
    let bind_addr = compose_bind_addr(&config);
    let (issue_tx, issue_rx) = mpsc::channel::<NormalizedIssue>(64);
    let webhook_state = Arc::new(WebhookState::new(webhook_secret.clone(), issue_tx));
    let listener = bind_listener(&bind_addr).await?;

    info!(
        target: "runtime.bootstrap",
        addr = %bind_addr,
        claude = %claude_binary.path().display(),
        "roki daemon bootstrap complete"
    );

    Ok(Bootstrapped {
        shutdown_signal,
        listener,
        bind_addr,
        webhook_state,
        issue_rx,
        _logging_guard: logging_guard,
        _signal_task: signal_task,
        components,
        orchestrator: composition.orchestrator,
        inbox: composition.inbox,
        read_handle: composition.read_handle,
        escalations: composition.escalations,
    })
}

// ---------------------------------------------------------------------------
// Orchestrator actor-map composition
// ---------------------------------------------------------------------------

/// Bundle of orchestrator-side trait objects + the read-side state map.
/// Construction is split between [`ProductionOrchestratorSeams::from_components`]
/// (production path) and the [`testing::compose_for_test`] helper (test path)
/// so test fixtures can inject recording stubs without forking the
/// composition step.
struct OrchestratorSeams {
    orchestrator_engine: Arc<dyn OrchestratorEngine>,
    phase_engine: Arc<dyn PhaseEngine>,
    worktree: Arc<dyn WorktreeOps>,
    session_dirs: Arc<dyn SessionDirOps>,
}

struct ProductionOrchestratorSeams;

impl ProductionOrchestratorSeams {
    fn from_components(components: &RuntimeComponents) -> OrchestratorSeams {
        let orchestrator_engine: Arc<dyn OrchestratorEngine> =
            Arc::new(OrchestratorEngineImpl::new(
                components.orchestrator_session_adapter.clone(),
                components.session_manager.clone(),
                components.workflow_policy.orchestrator.allowed_tools.clone(),
            ));

        // Phase pipeline placeholder: see `PendingPhaseEngine` doc-comment.
        // The adapter handle is retained on `RuntimeComponents` so 10.1.5
        // can wire the production phase pipeline without re-resolving the
        // claude binary.
        let phase_engine: Arc<dyn PhaseEngine> = Arc::new(PendingPhaseEngine::new());

        let worktree: Arc<dyn WorktreeOps> = components.worktree_manager.clone();
        let session_dirs: Arc<dyn SessionDirOps> = components.session_manager.clone();

        OrchestratorSeams {
            orchestrator_engine,
            phase_engine,
            worktree,
            session_dirs,
        }
    }
}

/// Result of [`compose_orchestrator`]. Held inside `Bootstrapped` so the
/// downstream composition steps (10.1.3 recovery, 10.1.4 tracker, 10.1.5
/// admission pipe, 10.1.6 shutdown) consume a single anchor.
struct OrchestratorComposition {
    orchestrator: Arc<Orchestrator>,
    inbox: OrchestratorInbox,
    read_handle: Arc<OrchestratorReadHandle>,
    escalations: Arc<EscalationQueue>,
    /// Shared between the orchestrator deps and the read handle. Held here
    /// so the integration test surface can observe per-issue snapshot rows
    /// without going through the read handle's sorting projection.
    state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>>,
}

/// Compose the orchestrator actor-map container from the supplied seams.
/// Wires:
///   - a fresh `EventBus` (default capacity);
///   - an empty `SubscriberHooks` registry (subscribers register later);
///   - a fresh `EscalationQueue`;
///   - an empty `state_map` shared between the orchestrator and the read
///     handle so the read handle observes every actor snapshot row;
///   - a fresh `OrchestratorReadHandle` projection;
///   - an `OrchestratorInbox` newtype around the `Arc<Orchestrator>` for
///     downstream admission/recovery wiring.
fn compose_orchestrator(seams: OrchestratorSeams) -> OrchestratorComposition {
    let event_bus = EventBus::new();
    let hooks = Arc::new(SubscriberHooks::new());
    let escalations = Arc::new(EscalationQueue::new());
    let state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>> =
        Arc::new(RwLock::new(HashMap::new()));

    let deps = OrchestratorDeps {
        orchestrator_engine: seams.orchestrator_engine,
        phase_engine: seams.phase_engine,
        worktree: seams.worktree,
        session_dirs: seams.session_dirs,
        event_bus,
        hooks,
        escalations: escalations.clone(),
        state_map: state_map.clone(),
    };

    let orchestrator = Arc::new(Orchestrator::new(deps));
    let inbox = OrchestratorInbox::new(orchestrator.clone());
    let read_handle = Arc::new(OrchestratorReadHandle::new(
        state_map.clone(),
        escalations.clone(),
    ));

    OrchestratorComposition {
        orchestrator,
        inbox,
        read_handle,
        escalations,
        state_map,
    }
}

/// Thin handle around the orchestrator actor-map container. Bridges the
/// admission pipe (10.1.5) and recovery seed (10.1.3) into the per-issue
/// actors via a single typed surface. Cheap to clone (one `Arc` bump per
/// clone).
#[derive(Clone)]
pub struct OrchestratorInbox {
    orchestrator: Arc<Orchestrator>,
}

impl OrchestratorInbox {
    pub fn new(orchestrator: Arc<Orchestrator>) -> Self {
        Self { orchestrator }
    }

    /// Forward `message` to the per-issue actor for `issue`. Spawns the
    /// actor on first message. Returns `Err(message)` if the actor's inbox
    /// is closed (terminal state already reached).
    pub async fn send(
        &self,
        issue: IssueId,
        message: ActorMessage,
    ) -> Result<(), ActorMessage> {
        self.orchestrator.send(issue, message).await
    }
}

/// Inputs to [`assemble_runtime_components`]. Kept as a typed bundle so the
/// factory shape stays additive as Tasks 10.1.2-10.1.6 introduce more
/// pre-resolved dependencies (orchestrator inbox, recovery seed, etc.).
struct RuntimeAssemblyInputs {
    claude_binary: ClaudeBinary,
    wt_path: PathBuf,
    ghq_path: PathBuf,
    workflow_policy: WorkflowPolicy,
    repos_allowlist: Vec<crate::config::RepoEntry>,
    permission_resolver: PermissionResolver,
}

/// Construct the engine adapters + managers from the resolved external
/// binaries, the loaded `WorkflowPolicy`, and the operator-controlled
/// permission resolver. Each factory call is wrapped so a refusal surfaces as
/// [`RuntimeError::ComponentAssembly`] naming the offending component.
///
/// The function is non-async + side-effect-free aside from `SessionManager`
/// default-root construction (which only reads the platform cache dir) so it
/// can be exercised in unit tests without a tokio runtime.
fn assemble_runtime_components(
    inputs: RuntimeAssemblyInputs,
) -> Result<RuntimeComponents, RuntimeError> {
    let RuntimeAssemblyInputs {
        claude_binary,
        wt_path,
        ghq_path,
        workflow_policy,
        repos_allowlist,
        permission_resolver,
    } = inputs;

    // Refuse early when the operator has not declared a phase-subprocess
    // permission strategy (Req 9.5). The `permission_resolver` argument is
    // already shaped from `Config` in production, but the gate is repeated
    // here so the test harness can force the missing-strategy path through
    // [`PermissionResolver::empty`].
    permission_resolver
        .ensure_phase_strategy_present()
        .map_err(|err: PermissionConfigError| RuntimeError::ComponentAssembly {
            component: "permission_resolver",
            message: err.to_string(),
        })?;

    let session_manager = Arc::new(SessionManager::new());

    let worktree_manager = Arc::new(WorktreeManager::new(
        Arc::new(RealWt::new()),
        Arc::new(RealGhq::new()),
        repos_allowlist,
    ));

    let orchestrator_session_adapter = Arc::new(OrchestratorSessionAdapter::new(
        claude_binary.clone(),
        PermissionResolver::resolve_for_orchestrator(
            &workflow_policy.orchestrator.allowed_tools,
        ),
    ));

    let phase_subprocess_adapter = Arc::new(PhaseSubprocessAdapter::new(
        claude_binary.clone(),
        permission_resolver.clone(),
    ));

    Ok(RuntimeComponents {
        claude_binary,
        wt_path,
        ghq_path,
        workflow_policy: Arc::new(workflow_policy),
        session_manager,
        worktree_manager,
        permission_resolver: Arc::new(permission_resolver),
        orchestrator_session_adapter,
        phase_subprocess_adapter,
    })
}

/// Build the operator-controlled phase-subprocess [`PermissionResolver`] from
/// the loaded [`Config`]. The resolver carries the canonical phase allowlist
/// baseline (Read + Bash + Edit) and the documented `--settings` sentinel
/// path under the session manager root; per-launch settings rendering is the
/// adapter's responsibility per `crates/roki-daemon/src/engine/phase_subprocess`.
fn build_permission_resolver(config: &Config) -> PermissionResolver {
    // The settings file path is a sentinel anchored under the user cache
    // directory so the per-launch renderer has a stable target. The file is
    // not required to exist at boot; the phase adapter writes it per launch.
    let settings_path = SessionManager::new().root().join("phase-allowlist.json");
    let phase_allowed_tools = vec!["Read".to_owned(), "Bash".to_owned(), "Edit".to_owned()];

    let base = PermissionResolver::with_settings_path(settings_path, phase_allowed_tools);
    match config.permissions.strategy {
        ConfigPermissionStrategy::SettingsAllowlist => base,
        ConfigPermissionStrategy::DangerouslySkipPermissions => {
            base.with_dangerously_skip_override(true)
        }
    }
}

/// Step 12: serve the webhook router and await shutdown. Bounds wind-down at
/// [`SHUTDOWN_WINDOW`] via [`await_workers_with_window`].
async fn serve(bootstrap: Bootstrapped) -> Result<(), RuntimeError> {
    let Bootstrapped {
        shutdown_signal,
        listener,
        bind_addr,
        webhook_state,
        mut issue_rx,
        ..
    } = bootstrap;

    let router = webhook_router(webhook_state);
    let shutdown_for_server = shutdown_signal.clone();
    let server = async move {
        let signal = shutdown_for_server.clone();
        let serve = axum::serve(listener, router);
        let serve_with_shutdown = serve.with_graceful_shutdown(async move {
            signal.wait().await;
        });
        if let Err(err) = serve_with_shutdown.await {
            warn!(
                target: "runtime.serve",
                error = %err,
                addr = %bind_addr,
                "webhook server exited with error"
            );
        }
    };

    let shutdown_for_drain = shutdown_signal.clone();
    let drain = async move {
        // Until the orchestrator bridge is wired in, drain the channel so
        // the webhook receiver does not back-pressure the axum handler.
        loop {
            tokio::select! {
                _ = shutdown_for_drain.wait() => return,
                msg = issue_rx.recv() => {
                    if msg.is_none() {
                        return;
                    }
                }
            }
        }
    };

    let server_handle = tokio::spawn(server);
    let drain_handle = tokio::spawn(drain);

    shutdown_signal.wait().await;

    let outcome = await_workers_with_window(
        [
            ("webhook-server".to_owned(), join_to_unit(server_handle)),
            ("issue-drain".to_owned(), join_to_unit(drain_handle)),
        ],
        SHUTDOWN_WINDOW,
    )
    .await;
    if !outcome.timed_out.is_empty() {
        warn!(
            target: "runtime.shutdown",
            timed_out = ?outcome.timed_out,
            "wind-down exceeded SHUTDOWN_WINDOW"
        );
    }
    Ok(())
}

async fn join_to_unit(handle: tokio::task::JoinHandle<()>) {
    let _ = handle.await;
}

/// Step 1 helper: locate the configuration file. `--config` wins; otherwise
/// look at `./roki.toml` per documented default.
fn resolve_config_path(cli_override: Option<&Path>) -> Result<PathBuf, RuntimeError> {
    if let Some(path) = cli_override {
        if !path.exists() {
            return Err(RuntimeError::ConfigFileMissing {
                path: path.to_path_buf(),
            });
        }
        return Ok(path.to_path_buf());
    }
    let default = PathBuf::from("roki.toml");
    if default.exists() {
        Ok(default)
    } else {
        Err(RuntimeError::ConfigFileMissing { path: default })
    }
}

/// Apply CLI-flag overrides on top of the loaded config. Each override is
/// scoped to the documented config column per `docs/reference/cli.md`.
fn apply_cli_overrides(config: &mut Config, args: &RunArgs) {
    if let Some(addr) = &args.bind {
        config.server.bind = Some(addr.clone());
    }
    if let Some(port) = args.port {
        config.server.port = Some(port);
    }
    if args.dangerously_skip_permissions {
        config.permissions.strategy =
            crate::config::PermissionStrategy::DangerouslySkipPermissions;
    }
}

fn init_logging_with_redaction(
    config: &Config,
    api_token: &SecretValue,
    webhook_secret: &SecretValue,
) -> Result<crate::logging::LoggingGuard, RuntimeError> {
    let logging_config = crate::logging::LoggingConfig {
        level: config
            .debug
            .level
            .clone()
            .unwrap_or_else(|| "info".to_owned()),
        destination: crate::logging::LogDestination::Stdout,
        json: true,
        redaction_secrets: vec![
            api_token.expose_secret().to_owned(),
            webhook_secret.expose_secret().to_owned(),
        ],
    };
    // The logging crate refuses double-init; smoke tests that drive
    // `bootstrap` repeatedly therefore silence the second/etc. failures so
    // each test stays self-contained. Production calls run() exactly once.
    match crate::logging::init(logging_config) {
        Ok(guard) => Ok(guard),
        Err(crate::logging::LoggingError::AlreadyInstalled) => {
            // Already initialized — the test harness or prior boot already
            // wired the subscriber. Surface a fresh guard sentinel.
            Ok(crate::logging::LoggingGuard::sentinel())
        }
        Err(err) => Err(RuntimeError::Logging(err)),
    }
}

/// Probe-only PATH check for an external binary. Production composition
/// uses [`resolve_external_binary`] so the resolved path can be threaded
/// into [`RuntimeComponents`]; this thinner helper is retained because the
/// `__roki_missing_binary__` regression test asserts the canonical refusal
/// shape directly.
#[cfg_attr(not(test), allow(dead_code))]
fn ensure_external_binary_on_path(name: &'static str) -> Result<(), RuntimeError> {
    if which_path(name).is_some() {
        Ok(())
    } else {
        Err(RuntimeError::ExternalBinaryMissing { name })
    }
}

/// Resolve an external binary on PATH and return its path. Refuses with the
/// canonical [`RuntimeError::ExternalBinaryMissing`] when missing — this is
/// the same refusal surface as [`ensure_external_binary_on_path`]; the
/// resolved path is propagated into [`RuntimeComponents`] so downstream
/// composition steps can shell out without re-walking PATH.
fn resolve_external_binary(name: &'static str) -> Result<PathBuf, RuntimeError> {
    which_path(name).ok_or(RuntimeError::ExternalBinaryMissing { name })
}

/// Minimal `which`-style PATH lookup (no extra dep). Honors the executable
/// bit on Unix and `.exe` on Windows.
fn which_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let target = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&target);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn compose_bind_addr(config: &Config) -> String {
    let host = config.server.bind.clone().unwrap_or_else(|| "127.0.0.1".to_owned());
    let port = config.server.port.unwrap_or(0);
    format!("{host}:{port}")
}

async fn bind_listener(addr: &str) -> Result<TokioTcpListener, RuntimeError> {
    // Bind through std first so the operator-facing error preserves the
    // original `io::Error` (tokio's bind helper hides errno on some
    // platforms). Then convert to a tokio listener.
    let std_listener = TcpListener::bind(addr).map_err(|source| RuntimeError::BindFailed {
        addr: addr.to_owned(),
        source,
    })?;
    std_listener
        .set_nonblocking(true)
        .map_err(|source| RuntimeError::BindFailed {
            addr: addr.to_owned(),
            source,
        })?;
    TokioTcpListener::from_std(std_listener).map_err(|source| RuntimeError::BindFailed {
        addr: addr.to_owned(),
        source,
    })
}

// ---------------------------------------------------------------------------
// Test composition entry point
// ---------------------------------------------------------------------------

/// Composition helpers exposed to integration tests under
/// `crates/roki-daemon/tests/`. Lives behind a public submodule rather than
/// `#[cfg(test)]` because the integration tests run against the
/// already-compiled library; the helpers here are intentionally narrow and
/// only construct the orchestrator actor-map composition (10.1.2 surface)
/// without binding any sockets or starting tracker tasks.
pub mod testing {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    use crate::orchestrator::core::{
        Orchestrator, OrchestratorEngine, PhaseEngine, SessionDirOps, WorktreeOps,
    };
    use crate::orchestrator::escalation::EscalationQueue;
    use crate::orchestrator::read::{ActorSnapshot, OrchestratorReadHandle};
    use crate::orchestrator::state::IssueId;

    use super::{
        OrchestratorComposition, OrchestratorInbox, OrchestratorSeams, compose_orchestrator,
    };

    /// Caller-supplied seams for [`compose_for_test`]. Tests inject
    /// recording stubs (see `tests/common/mod.rs`) under each `Arc<dyn ...>`
    /// surface so they can assert engine invocations + state-map snapshots
    /// without spawning any real subprocess.
    pub struct RuntimeTestSeams {
        pub orchestrator_engine: Arc<dyn OrchestratorEngine>,
        pub phase_engine: Arc<dyn PhaseEngine>,
        pub worktree: Arc<dyn WorktreeOps>,
        pub session_dirs: Arc<dyn SessionDirOps>,
    }

    /// Result of [`compose_for_test`]. Mirrors the orchestrator-composition
    /// half of `Bootstrapped` so integration tests can assert on the actor
    /// map shape directly.
    pub struct ComposedHarness {
        pub engine: Arc<dyn OrchestratorEngine>,
        pub phase: Arc<dyn PhaseEngine>,
        pub worktree: Arc<dyn WorktreeOps>,
        pub session_dirs: Arc<dyn SessionDirOps>,
        pub orchestrator: Arc<Orchestrator>,
        pub inbox: OrchestratorInbox,
        pub state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>>,
        pub escalations: Arc<EscalationQueue>,
        pub read_handle: Arc<OrchestratorReadHandle>,
        /// Lifetime anchors (e.g. tempdirs) the caller wants to keep alive
        /// alongside the harness. Stored as opaque drop guards so the
        /// runtime crate need not depend on `tempfile` at build time.
        pub lifetime_anchors: Vec<Box<dyn std::any::Any + Send + Sync>>,
    }

    /// Compose an orchestrator actor-map around the supplied seams.
    /// Mirrors the production composition step that runs inside
    /// `runtime::bootstrap`, so tests exercise the same wiring shape.
    pub fn compose_for_test(seams: RuntimeTestSeams) -> ComposedHarness {
        let engine = seams.orchestrator_engine.clone();
        let phase = seams.phase_engine.clone();
        let worktree = seams.worktree.clone();
        let session_dirs = seams.session_dirs.clone();

        let composition = compose_orchestrator(OrchestratorSeams {
            orchestrator_engine: engine.clone(),
            phase_engine: phase.clone(),
            worktree: worktree.clone(),
            session_dirs: session_dirs.clone(),
        });
        let OrchestratorComposition {
            orchestrator,
            inbox,
            read_handle,
            escalations,
            state_map,
        } = composition;

        ComposedHarness {
            engine,
            phase,
            worktree,
            session_dirs,
            orchestrator,
            inbox,
            state_map,
            escalations,
            read_handle,
            lifetime_anchors: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RunArgs;
    use crate::config::StaticEnv;

    /// Default-flag RunArgs with the `--config` slot left empty so each test
    /// can fill in its own config-file path.
    fn args(config_path: Option<PathBuf>) -> RunArgs {
        RunArgs {
            config: config_path,
            bind: None,
            port: None,
            dangerously_skip_permissions: false,
            debug: false,
        }
    }

    fn env_with_secrets() -> StaticEnv {
        StaticEnv::new()
            .set("LINEAR_API_TOKEN", "lin_api_token_for_tests")
            .set("LINEAR_WEBHOOK_SECRET", "webhook_secret_for_tests")
    }

    #[tokio::test]
    async fn missing_config_file_yields_actionable_refusal() {
        let env = env_with_secrets();
        let path = std::env::temp_dir().join("roki-runtime-missing-config.toml");
        // Ensure absence regardless of prior test runs.
        let _ = std::fs::remove_file(&path);
        let err = run_with_env(args(Some(path.clone())), &env).await.unwrap_err();
        match &err {
            RuntimeError::ConfigFileMissing { path: reported } => {
                assert_eq!(reported, &path);
            }
            other => panic!("expected ConfigFileMissing, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("--config"),
            "remediation must mention --config: {msg}"
        );
    }

    #[test]
    fn cli_overrides_replace_config_server_values() {
        // Direct unit test of the overrides shape so the bootstrap step that
        // applies them stays separately verifiable.
        let dir = tempfile::TempDir::new().unwrap();
        let workflow = dir.path().join("WORKFLOW.md");
        std::fs::write(&workflow, "stub").unwrap();
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
port = 8080

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let mut config = Config::load_from_str(&body).unwrap();
        let overridden = RunArgs {
            config: None,
            bind: Some("0.0.0.0".to_owned()),
            port: Some(9100),
            dangerously_skip_permissions: true,
            debug: false,
        };
        apply_cli_overrides(&mut config, &overridden);
        assert_eq!(config.server.bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(config.server.port, Some(9100));
        assert!(matches!(
            config.permissions.strategy,
            crate::config::PermissionStrategy::DangerouslySkipPermissions
        ));
    }

    #[test]
    fn compose_bind_addr_uses_documented_default_host() {
        let dir = tempfile::TempDir::new().unwrap();
        let workflow = dir.path().join("WORKFLOW.md");
        std::fs::write(&workflow, "stub").unwrap();
        let body = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let mut config = Config::load_from_str(&body).unwrap();
        config.server.port = Some(7000);
        assert_eq!(compose_bind_addr(&config), "127.0.0.1:7000");
        config.server.bind = Some("0.0.0.0".to_owned());
        assert_eq!(compose_bind_addr(&config), "0.0.0.0:7000");
    }

    #[test]
    fn ensure_external_binary_on_path_refuses_missing() {
        // Unique sentinel name guaranteed not to exist on PATH.
        let err = ensure_external_binary_on_path("__roki_missing_binary__").unwrap_err();
        match &err {
            RuntimeError::ExternalBinaryMissing { name } => {
                assert_eq!(*name, "__roki_missing_binary__");
            }
            other => panic!("expected ExternalBinaryMissing, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("__roki_missing_binary__"),
            "missing-binary error must name the missing executable: {msg}"
        );
        assert!(
            msg.contains("PATH"),
            "missing-binary error must mention PATH remediation: {msg}"
        );
    }

    // ----- RuntimeComponents assembly (Task 10.1.1) -----

    fn fixture_workflow_policy() -> WorkflowPolicy {
        // Drive through the public parse + validate path so the policy
        // mirrors what the production loader would emit for the canonical
        // four-block workflow shape.
        let body = "---\n---\n\
                    ## prompt_template_orchestrator\norch body\n\
                    \n## prompt_template_implement_direct\nimpl body\n\
                    \n## prompt_template_validate_direct\nval body\n\
                    \n## prompt_template_open_pr\nopen body\n";
        let parsed = crate::workflow::parse::parse_str(body).expect("parse fixture workflow");
        crate::workflow::schema::validate(parsed).expect("validate fixture workflow")
    }

    fn fixture_resolver() -> PermissionResolver {
        PermissionResolver::with_settings_path(
            PathBuf::from("/tmp/roki-runtime-test-phase-allowlist.json"),
            vec!["Read".to_owned(), "Bash".to_owned(), "Edit".to_owned()],
        )
    }

    fn fake_external_binary(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    fn fixture_inputs(dir: &Path) -> RuntimeAssemblyInputs {
        let claude = fake_external_binary(dir, "claude");
        let claude_binary = ClaudeBinary::discover(Some(&claude)).expect("fake claude");
        let wt = fake_external_binary(dir, "wt");
        let ghq = fake_external_binary(dir, "ghq");
        RuntimeAssemblyInputs {
            claude_binary,
            wt_path: wt,
            ghq_path: ghq,
            workflow_policy: fixture_workflow_policy(),
            repos_allowlist: vec![],
            permission_resolver: fixture_resolver(),
        }
    }

    #[test]
    fn assemble_runtime_components_yields_non_none_adapters_and_managers() {
        let dir = tempfile::TempDir::new().unwrap();
        let inputs = fixture_inputs(dir.path());
        let components = assemble_runtime_components(inputs).expect("assembly succeeds");

        // Each adapter / manager arrived as a populated Arc handle: every
        // Arc::strong_count must be at least 1, and the workflow policy must
        // round-trip the orchestrator allowlist surfaced via the loader.
        assert!(Arc::strong_count(&components.session_manager) >= 1);
        assert!(Arc::strong_count(&components.worktree_manager) >= 1);
        assert!(Arc::strong_count(&components.permission_resolver) >= 1);
        assert!(Arc::strong_count(&components.orchestrator_session_adapter) >= 1);
        assert!(Arc::strong_count(&components.phase_subprocess_adapter) >= 1);
        assert!(Arc::strong_count(&components.workflow_policy) >= 1);
        assert!(!components.workflow_policy.orchestrator.allowed_tools.is_empty());
        assert!(components.claude_binary.path().exists());
        assert!(components.wt_path.exists());
        assert!(components.ghq_path.exists());
    }

    #[test]
    fn assemble_runtime_components_refuses_when_permission_resolver_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut inputs = fixture_inputs(dir.path());
        // Force the permission-resolver factory error path: an `empty`
        // resolver carries neither a settings strategy nor a dangerous-skip
        // override, which is the exact missing-strategy refusal documented
        // in Req 9.5.
        inputs.permission_resolver = PermissionResolver::empty();
        let err = assemble_runtime_components(inputs).unwrap_err();
        match &err {
            RuntimeError::ComponentAssembly { component, message } => {
                assert_eq!(*component, "permission_resolver");
                assert!(
                    message.contains("permission strategy"),
                    "remediation must mention permission strategy: {message}",
                );
            }
            other => panic!("expected ComponentAssembly, got {other:?}"),
        }
        let rendered = err.to_string();
        assert!(
            rendered.contains("permission_resolver"),
            "refusal must name the offending component: {rendered}",
        );
    }

    #[test]
    fn build_permission_resolver_honors_dangerously_skip_strategy_from_config() {
        // Workflow body shared across both branches.
        let dir = tempfile::TempDir::new().unwrap();
        let workflow = dir.path().join("WORKFLOW.md");
        std::fs::write(&workflow, "stub").unwrap();

        let body_skip = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "dangerously-skip"
"#,
            workflow.display()
        );
        let cfg_skip = Config::load_from_str(&body_skip).unwrap();
        let resolver_skip = build_permission_resolver(&cfg_skip);
        // Dangerous-skip override path: the resolver yields the
        // `--dangerously-skip-permissions` strategy for non-Classify phases.
        let resolved = resolver_skip
            .resolve_for_phase(crate::engine::phase_subprocess::catalog::PhaseName::Implement)
            .unwrap();
        assert!(matches!(
            resolved.strategy,
            crate::permissions::PermissionStrategy::DangerouslySkipPermissions,
        ));

        let body_allow = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let cfg_allow = Config::load_from_str(&body_allow).unwrap();
        let resolver_allow = build_permission_resolver(&cfg_allow);
        let resolved_allow = resolver_allow
            .resolve_for_phase(crate::engine::phase_subprocess::catalog::PhaseName::Implement)
            .unwrap();
        assert!(matches!(
            resolved_allow.strategy,
            crate::permissions::PermissionStrategy::SettingsAllowlist { .. },
        ));
    }
}

