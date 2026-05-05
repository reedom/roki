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
use std::time::Duration;

use thiserror::Error;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::cli::RunArgs;
use crate::config::{
    AssigneeSpec, Config, ConfigError, EnvReader, PermissionStrategy as ConfigPermissionStrategy,
    ProcessEnv, SecretValue,
};
use crate::engine::claude::{ClaudeBinary, ClaudeError};
use crate::engine::orchestrator_session::adapter::OrchestratorSessionAdapter;
use crate::engine::orchestrator_session::engine_impl::OrchestratorEngineImpl;
use crate::engine::phase_subprocess::adapter::PhaseSubprocessAdapter;
use crate::engine::phase_subprocess::engine_impl::PhaseSubprocessEngineImpl;
use crate::exec::ghq::{GhqTool, RealGhq};
use crate::exec::wt::{RealWt, WtTool};
use crate::orchestrator::core::{
    ActorMessage, Orchestrator, OrchestratorDeps, OrchestratorEngine, PhaseEngine, SessionDirOps,
    WorktreeOps,
};
use crate::orchestrator::escalation::{
    EscalationEntry, EscalationKind, EscalationQueue,
};
use crate::orchestrator::events::EventBus;
use crate::orchestrator::hooks::SubscriberHooks;
use crate::orchestrator::read::{ActorSnapshot, OrchestratorReadHandle};
use crate::orchestrator::recovery::{
    DiscoveredIssue, RecoveryDecision, RecoveryError, RecoveryReconciler,
};
use crate::orchestrator::state::{InactiveReason, IssueId, Mode, WorkerState};
use crate::orchestrator::tracker_bridge::{DedupIndex, ObserveOutcome, TerminationReason};
use crate::permissions::{PermissionConfigError, PermissionResolver};
use crate::session::SessionManager;
use crate::shutdown::{
    self, AwaitOutcome, SHUTDOWN_WINDOW, ShutdownSignal, ShutdownTrigger,
    await_workers_with_window, install_signal_handlers,
};
use crate::tracker::linear::{LinearClient, LinearError, LinearPoller};
use crate::tracker::model::{LinearStateName, LinearUserId, NormalizedIssue};
use crate::tracker::pre_admission::PreAdmissionJudge;
use crate::tracker::refresh::{LinearTrackerHandle, TrackerRefresh};
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

    /// Linear `viewer` lookup failed while resolving `[linear].assignee = "me"`.
    /// The daemon refuses to start so the operator sees one log line naming
    /// the upstream cause rather than per-issue admission churn at runtime.
    #[error(
        "Linear `viewer` lookup failed while resolving `[linear].assignee = \"me\"`: \
         {source}; verify the API token and network reachability"
    )]
    AssigneeViewerLookup {
        #[source]
        source: LinearError,
    },

    /// Restart-recovery scan failed before the bootstrap could seed the
    /// orchestrator actor map. The error surfaces the offending session-root
    /// path or repo id so the operator log entry points at exactly one cause.
    #[error("restart-recovery scan failed: {source}")]
    RecoverySeed {
        #[source]
        source: RecoveryError,
    },

    /// Restart-recovery exceeded the configured window. Refuses to start so
    /// a hung Linear instance does not block the daemon indefinitely; the
    /// elapsed bound is included in the message for the operator log.
    #[error(
        "restart-recovery scan did not complete within {elapsed:?}; refusing to start"
    )]
    RecoveryTimedOut { elapsed: std::time::Duration },
}

/// Maximum time the bootstrap waits for `RecoveryReconciler` to complete its
/// scan + decide pass before refusing to start. Set conservatively so
/// transient Linear slowness does not block the daemon indefinitely; the
/// scan is read-only so re-running on next start is safe.
pub const RECOVERY_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Top-level entry point dispatched by `main.rs`. Drives the documented
/// 12-step bootstrap composition.
pub async fn run(args: RunArgs) -> Result<(), RuntimeError> {
    let env = ProcessEnv;
    run_with_env(args, &env).await
}

/// Test-friendly variant that accepts an injected [`EnvReader`].
///
/// Production callers route through [`run`]. The bootstrap step now
/// returns `(Bootstrapped, ShutdownTrigger)` so the OS-signal-handler
/// install lives at this layer rather than inside `bootstrap`. The
/// `runtime::testing::bootstrap_for_test` seam (Task 10.5) skips the
/// signal-handler install and hands the trigger back to the test instead,
/// so e2e tests can wind the daemon down without delivering SIGINT to the
/// test harness process.
pub async fn run_with_env(args: RunArgs, env: &dyn EnvReader) -> Result<(), RuntimeError> {
    let (bootstrap, shutdown_trigger) = bootstrap(args, env).await?;
    // Production-only: install OS signal handlers wired to the trigger the
    // bootstrap returned. The handler task fires the trigger on the first
    // SIGINT/SIGTERM. The returned `JoinHandle` is held until `serve()`
    // returns so the task is not aborted prematurely.
    let _signal_task = install_signal_handlers(shutdown_trigger);
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
    /// Read-only Linear GraphQL client constructed during step 8 and shared
    /// with the recovery seed + (in 10.1.4) the workspace-level
    /// `LinearTracker` poller. Held here so the poller wiring can clone the
    /// same `Arc` without re-constructing the backoff state.
    #[allow(dead_code)]
    linear_client: Arc<LinearClient>,
    /// Resolved pre-admission judge (assignee + admit_states); shared with
    /// 10.1.5's pre-admission funnel.
    #[allow(dead_code)]
    judge: Arc<PreAdmissionJudge>,
    /// Workspace-level tracker handle: owns the spawned poller's join
    /// handle and the `TrackerRefresh` nudge endpoint. Held so 10.1.5 can
    /// share the refresh handle with the orchestrator session, 10.1.6 can
    /// await the poller within `SHUTDOWN_WINDOW`, and observability can
    /// clone the refresh `Arc<dyn TrackerRefresh>` without rewiring.
    tracker_handle: TrackerHandle,
    /// Per-issue dedup index that absorbs duplicate webhook + poll
    /// observations and decides whether each new observation should launch
    /// a fresh orchestrator session, refresh the in-flight snapshot, drop
    /// silently, or terminate the in-flight session. Constructed empty at
    /// bootstrap; observations land via the admission pipe (10.1.5).
    /// Held so 10.1.6 (shutdown) and observability can read snapshots.
    #[allow(dead_code)]
    dedup: Arc<DedupIndex>,
}

/// Workspace-level tracker handle composed by step 9 of the daemon
/// bootstrap. Carries the `TrackerRefresh` nudge endpoint and the spawned
/// poller's `JoinHandle` so wind-down can await the poller alongside the
/// webhook server inside `SHUTDOWN_WINDOW`.
pub(crate) struct TrackerHandle {
    /// `Arc<dyn TrackerRefresh>` so downstream callers (10.1.5 / 10.1.6 /
    /// observability) clone this value cheaply without re-constructing the
    /// underlying watch sender or backoff peek.
    pub(crate) refresh: Arc<dyn TrackerRefresh>,
    /// Join handle for the spawned poller task. Awaited in `serve()` under
    /// the shared `SHUTDOWN_WINDOW`; the poller exits when the shared
    /// `ShutdownSignal` fires.
    pub(crate) poller_join: tokio::task::JoinHandle<()>,
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
    /// Optional per-issue debug-sink factory composed when the operator
    /// passes `--debug` or sets `[debug].dir` (Req 11.6, 11.7). When
    /// `Some`, the engine adapters' launch contexts materialize a
    /// per-issue [`crate::logging::PerIssueDebugSink`] for every spawn so
    /// stdout / stderr lines are appended to `<dir>/<issue>.log`. `None`
    /// disables the per-issue capture.
    pub debug_sink_factory: Option<Arc<crate::logging::DebugSinkFactory>>,
}

/// Step 1-11 of the daemon bootstrap. Step 12 (the `tokio::select!` wind-down)
/// is the caller's responsibility — production routes through [`serve`];
/// tests skip the await loop and call [`Bootstrapped`] field-by-field.
///
/// The bootstrap constructs the paired `(ShutdownSignal, ShutdownTrigger)`
/// and returns the trigger to the caller rather than installing OS signal
/// handlers itself. Production `run_with_env` immediately wires the
/// trigger to the SIGINT/SIGTERM handlers via [`install_signal_handlers`];
/// the `runtime::testing::bootstrap_for_test` seam (Task 10.5) skips that
/// step and lets the caller fire the trigger directly so e2e tests can
/// wind down without delivering OS signals to the test harness process.
async fn bootstrap(
    args: RunArgs,
    env: &dyn EnvReader,
) -> Result<(Bootstrapped, ShutdownTrigger), RuntimeError> {
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

    // ---- Step 6: construct the shared shutdown channel. The trigger is
    // returned to the caller — production `run_with_env` wires it to the
    // OS signal handlers via [`install_signal_handlers`]; the
    // `runtime::testing::bootstrap_for_test` seam keeps the trigger so
    // tests fire shutdown directly. Either way, every
    // `ShutdownSignal::wait()` subscriber wakes when the trigger fires.
    let (shutdown_signal, shutdown_trigger) = shutdown::new();

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
    let debug_sink_factory = compose_debug_sink_factory(&args, &config);
    let components = assemble_runtime_components(RuntimeAssemblyInputs {
        claude_binary: claude_binary.clone(),
        wt_path: wt_path.clone(),
        ghq_path: ghq_path.clone(),
        workflow_policy,
        repos_allowlist: config.repos.clone(),
        permission_resolver,
        debug_sink_factory,
    })?;

    // ---- Step 8 (continued): compose the orchestrator actor map around
    // the assembled `RuntimeComponents`. The phase pipeline is wired
    // through `PhaseSubprocessEngineImpl` so each `run_phase` action
    // reaches the bounded `PhaseSubprocessAdapter::spawn` plus the
    // documented exit translation.
    let orchestrator_seams = ProductionOrchestratorSeams::from_components(&components);
    let composition = compose_orchestrator(orchestrator_seams);

    // ---- Step 8 (continued): build the read-only Linear client + resolved
    // viewer + pre-admission judge that recovery (and 10.1.4 poller) share.
    // Endpoint comes from the validated `[linear].endpoint` slot (defaulted
    // to `DEFAULT_LINEAR_ENDPOINT` at config load); the slot exists so
    // operators can target Linear's EU endpoint or a self-hosted GraphQL
    // proxy, and so e2e tests can redirect production bootstrap at a
    // wiremock without modifying this composition path.
    let linear_client = Arc::new(LinearClient::new(
        config.linear.endpoint.clone(),
        api_token.clone(),
    ));
    let viewer = match &config.linear.assignee {
        AssigneeSpec::Me => linear_client
            .viewer()
            .await
            .map_err(|source| RuntimeError::AssigneeViewerLookup { source })?,
        // The Selector path is already refused at step 4; this arm exists
        // for exhaustiveness and to surface `Selector(_)` again should the
        // step-4 gate ever loosen without updating this branch.
        AssigneeSpec::Selector(value) => {
            return Err(RuntimeError::AssigneeResolve {
                selector: value.clone(),
            });
        }
    };
    let admit_states: std::collections::BTreeSet<LinearStateName> = config
        .linear
        .admit_states
        .iter()
        .map(|name| LinearStateName::from(name.as_str()))
        .collect();
    let judge = Arc::new(PreAdmissionJudge::new(viewer.clone(), admit_states));

    // ---- Step 8 (continued): drive the restart-recovery scan + decide
    // pass via `RecoveryReconciler` against the same on-disk world the
    // orchestrator + worktree-manager observe. The scan is read-only; the
    // bootstrap blocks until it completes (or `RECOVERY_WINDOW` elapses).
    let reconciler = RecoveryReconciler::new(
        components.session_manager.root().to_path_buf(),
        config.repos.clone(),
        Arc::new(RealWt::new()),
        Arc::new(RealGhq::new()),
    )
    .map_err(|source| RuntimeError::RecoverySeed { source })?;
    let production_sink = ProductionSeedSink {
        inbox: composition.inbox.clone(),
        escalations: composition.escalations.clone(),
        state_map: composition.state_map.clone(),
    };
    drive_recovery_seed(
        &reconciler,
        &linear_client,
        judge.as_ref(),
        &production_sink,
        RECOVERY_WINDOW,
        Vec::new(),
    )
    .await?;

    // ---- Step 11 (anchor): build the single workspace-level mpsc that
    // both the webhook receiver (Task 10.3) and the poller (this step,
    // 10.1.4) feed into. Constructing the channel up front so the poller's
    // sink shares the same `issue_rx` consumer as the webhook drain.
    let bind_addr = compose_bind_addr(&config);
    let (issue_tx, issue_rx) = mpsc::channel::<NormalizedIssue>(64);
    let webhook_state = Arc::new(WebhookState::new(webhook_secret.clone(), issue_tx.clone()));

    // ---- Step 9: start the workspace-level `LinearTracker` poller. The
    // poller shares the same backoff curve as the recovery client (so a
    // 429 surfaced during recovery still suppresses subsequent polls), the
    // resolved viewer assignee, and the resolved admit_states. The cadence
    // floor comes from `[linear].poll_cadence_seconds` (default 300s; the
    // loader refuses anything below `MIN_POLL_CADENCE_SECONDS`).
    let cadence_floor = std::time::Duration::from_secs(config.linear.poll_cadence_seconds);
    let tracker_handle = spawn_workspace_poller(
        (*linear_client).clone(),
        viewer.clone(),
        admit_states_to_states_vec(&config.linear.admit_states),
        cadence_floor,
        issue_tx.clone(),
        shutdown_signal.clone(),
    );

    // ---- Step 10-11: bind webhook listener. Hard-refuse on port conflict.
    let listener = bind_listener(&bind_addr).await?;

    info!(
        target: "runtime.bootstrap",
        addr = %bind_addr,
        claude = %claude_binary.path().display(),
        "roki daemon bootstrap complete"
    );

    // ---- Step 11 (admission pipe anchor): construct the per-issue dedup
    // index now so it is held on `Bootstrapped` for downstream wiring +
    // observability. The recovery seed (Task 10.1.3) does not touch the
    // dedup index — per-issue snapshot rehydration is a follow-up; the
    // DedupIndex tests cover that path independently.
    let dedup = Arc::new(DedupIndex::new());

    Ok((
        Bootstrapped {
            shutdown_signal,
            listener,
            bind_addr,
            webhook_state,
            issue_rx,
            _logging_guard: logging_guard,
            components,
            orchestrator: composition.orchestrator,
            inbox: composition.inbox,
            read_handle: composition.read_handle,
            escalations: composition.escalations,
            linear_client,
            judge,
            tracker_handle,
            dedup,
        },
        shutdown_trigger,
    ))
}

// ---------------------------------------------------------------------------
// Workspace-level tracker poller (Task 10.1.4)
// ---------------------------------------------------------------------------

/// Convert the loader's resolved `BTreeSet<String>` into the
/// `Vec<LinearStateName>` shape `LinearPoller::new` consumes.
fn admit_states_to_states_vec(
    admit_states: &std::collections::BTreeSet<String>,
) -> Vec<LinearStateName> {
    admit_states
        .iter()
        .map(|name| LinearStateName::from(name.as_str()))
        .collect()
}

/// Construct + spawn the single workspace-level [`LinearPoller`] and a
/// paired [`LinearTrackerHandle`]. The handle and the poller share the same
/// `BackoffState` (read off `client.backoff()`) so a 429 surfaced by either
/// path immediately gates the other; the same `mpsc::Sender` feeds both the
/// poller and the webhook receiver into the single workspace-level
/// `issue_rx` consumer (Task 10.1.5 admission funnel).
///
/// Spawned via `tokio::spawn` so the bootstrap can return synchronously; the
/// returned [`TrackerHandle`] holds the poller's `JoinHandle` so 10.1.6 can
/// await wind-down within `SHUTDOWN_WINDOW`.
fn spawn_workspace_poller(
    client: LinearClient,
    assignee: LinearUserId,
    states: Vec<LinearStateName>,
    cadence_floor: std::time::Duration,
    sink: mpsc::Sender<NormalizedIssue>,
    shutdown_signal: ShutdownSignal,
) -> TrackerHandle {
    let backoff = client.backoff();
    let (handle, refresh_rx) = LinearTrackerHandle::paired(cadence_floor, backoff);
    let refresh: Arc<dyn TrackerRefresh> = Arc::new(handle);

    let poller = LinearPoller::new(client, assignee, states, cadence_floor, sink, refresh_rx);
    let poller_join = tokio::spawn(poller.run(shutdown_signal));

    TrackerHandle {
        refresh,
        poller_join,
    }
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
                components.debug_sink_factory.clone(),
            ));

        // Production phase pipeline: `PhaseSubprocessEngineImpl` composes
        // the bounded `PhaseSubprocessAdapter` with the documented exit
        // translation, threading the workflow policy + permission resolver
        // captured in `RuntimeComponents` and the optional per-issue
        // debug-sink factory (Req 11.6 / 11.7) into every spawn.
        let phase_engine: Arc<dyn PhaseEngine> = Arc::new(PhaseSubprocessEngineImpl::new(
            components.phase_subprocess_adapter.clone(),
            components.workflow_policy.clone(),
            (*components.permission_resolver).clone(),
            components.debug_sink_factory.clone(),
        ));

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

// ---------------------------------------------------------------------------
// Recovery seed driver (Task 10.1.3)
// ---------------------------------------------------------------------------

/// Sink consuming the per-issue decisions emitted by the recovery scan. The
/// production sink seeds the orchestrator actor map + escalation queue; the
/// integration-test sink records observations into in-memory buffers. Both
/// share the [`drive_recovery_seed`] driver so the same `scan` + `decide`
/// composition runs in tests as in production.
#[async_trait::async_trait]
pub(crate) trait RecoverySeedSink: Send + Sync {
    /// Seed an admit (covers `ResumeActive` + `FreshQueued`).
    async fn admit(&self, issue: IssueId, mode: Mode);
    /// Retain an orphan (covers `OrphanedSession` + `OrphanedWorktree`).
    /// `discovered` is the original on-disk evidence so the sink can build
    /// the structured-fields blob and the snapshot row consistently.
    async fn orphan(&self, issue: IssueId, discovered: &DiscoveredIssue);
    /// Skip cell (`NoOp`). Sinks may log; production logs at info level.
    async fn noop(&self, issue: IssueId);
}

/// Production sink: seeds the orchestrator inbox with `TrackerAdmit`,
/// enqueues `EscalationKind::Orphan` entries, and writes
/// `Inactive(InactiveReason::Orphan)` rows into the shared state map so
/// `OrchestratorRead::snapshot` reflects the orphan retention.
struct ProductionSeedSink {
    inbox: OrchestratorInbox,
    escalations: Arc<EscalationQueue>,
    state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>>,
}

#[async_trait::async_trait]
impl RecoverySeedSink for ProductionSeedSink {
    async fn admit(&self, issue: IssueId, mode: Mode) {
        // The production recovery seed sends Admit with `repo: None`; the
        // worktree allowlist is re-resolved on the orchestrator's first
        // non-classify phase nomination per design.md.
        if let Err(returned) = self
            .inbox
            .send(issue.clone(), ActorMessage::TrackerAdmit { mode, repo: None })
            .await
        {
            // The actor's inbox should never be closed on first message; log
            // and drop so the bootstrap proceeds. The actor can be revived on
            // the next webhook / poll observation.
            warn!(
                target: "runtime.recovery",
                issue = %issue,
                "actor inbox closed before recovery admit landed; dropping {:?}",
                returned,
            );
        }
    }

    async fn orphan(&self, issue: IssueId, discovered: &DiscoveredIssue) {
        let fields = RecoveryReconciler::<RealWt, RealGhq>::orphan_directive_fields(discovered);
        let entry = EscalationEntry {
            issue: issue.clone(),
            repo: discovered
                .worktrees
                .first()
                .map(|w| w.repo_id.0.clone()),
            kind: EscalationKind::Orphan,
            correlation_id: format!("recovery-{issue}"),
            timestamp: time::OffsetDateTime::now_utc(),
            structured_fields: fields,
        };
        self.escalations.enqueue(entry).await;
        // Reflect the orphan retention in the read-side state map. The actor
        // map is NOT spawned for orphans — the escalation queue + snapshot
        // row are the only surfaces.
        if let Ok(mut map) = self.state_map.write() {
            map.insert(
                issue.clone(),
                ActorSnapshot {
                    issue,
                    state: WorkerState::Inactive(InactiveReason::Orphan),
                    mode: None,
                    latest_linear_state: None,
                },
            );
        }
    }

    async fn noop(&self, issue: IssueId) {
        info!(
            target: "runtime.recovery",
            issue = %issue,
            "recovery NoOp (Linear terminal + nothing on disk)"
        );
    }
}

/// Drive `scan + decide` for every discovered issue, dispatching each
/// decision through `sink`. Bounded by `window` via `tokio::time::timeout`;
/// on timeout returns [`RuntimeError::RecoveryTimedOut`]. Reconciler errors
/// are mapped to [`RuntimeError::RecoverySeed`] so the operator log line
/// names the offending session-root or repo id.
async fn drive_recovery_seed<W, G, S>(
    reconciler: &RecoveryReconciler<W, G>,
    linear: &LinearClient,
    judge: &PreAdmissionJudge,
    sink: &S,
    window: std::time::Duration,
    extra_decisions: Vec<RecoveryDecision>,
) -> Result<(), RuntimeError>
where
    W: WtTool,
    G: GhqTool,
    S: RecoverySeedSink + ?Sized,
{
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(window, async {
        let discovered_set = reconciler
            .scan()
            .await
            .map_err(|source| RuntimeError::RecoverySeed { source })?;

        for discovered in discovered_set {
            let decision = reconciler
                .decide(discovered.clone(), linear, judge)
                .await
                .map_err(|source| RuntimeError::RecoverySeed { source })?;
            dispatch_decision(decision, &discovered, sink).await;
        }

        for decision in extra_decisions {
            // Synthetic decisions (test seam + future poller-fed seeds) need
            // an empty `discovered` shell so the orphan branch can render
            // structured fields without on-disk evidence.
            let synthetic = DiscoveredIssue {
                issue: decision_issue(&decision).clone(),
                session_present: false,
                worktrees: Vec::new(),
            };
            dispatch_decision(decision, &synthetic, sink).await;
        }

        Ok::<(), RuntimeError>(())
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_elapsed) => Err(RuntimeError::RecoveryTimedOut {
            elapsed: started.elapsed(),
        }),
    }
}

fn decision_issue(decision: &RecoveryDecision) -> &IssueId {
    match decision {
        RecoveryDecision::ResumeActive { issue, .. }
        | RecoveryDecision::FreshQueued { issue, .. }
        | RecoveryDecision::OrphanedSession { issue }
        | RecoveryDecision::OrphanedWorktree { issue }
        | RecoveryDecision::NoOp { issue } => issue,
    }
}

async fn dispatch_decision<S>(
    decision: RecoveryDecision,
    discovered: &DiscoveredIssue,
    sink: &S,
) where
    S: RecoverySeedSink + ?Sized,
{
    match decision {
        RecoveryDecision::ResumeActive { issue, mode }
        | RecoveryDecision::FreshQueued { issue, mode } => {
            sink.admit(issue, mode).await;
        }
        RecoveryDecision::OrphanedSession { issue }
        | RecoveryDecision::OrphanedWorktree { issue } => {
            sink.orphan(issue, discovered).await;
        }
        RecoveryDecision::NoOp { issue } => {
            sink.noop(issue).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Admission pipe (Task 10.1.5)
// ---------------------------------------------------------------------------
//
// Webhook + poll observations are funnelled through the workspace-level mpsc
// (`issue_rx`). Each observation runs through `PreAdmissionJudge::evaluate`
// (which already emits `tracker.pre_admission.skipped` on Skip), then through
// `DedupIndex::observe` to decide whether to launch a fresh actor, refresh
// the in-flight snapshot, drop silently, or terminate the in-flight session.
//
// Concurrency: `DedupIndex` serialises observations through an internal
// `RwLock`, so two concurrent observations of the same issue cannot both
// produce `LaunchFresh`; the second sees the in-flight entry seeded by the
// first and returns `UpdateInPlace`.

/// Sink the admission pipe routes `ActorMessage`s through. The production
/// implementation is [`OrchestratorInbox`]; tests inject a recording sink to
/// observe the routed message sequence without spawning per-issue actors.
#[async_trait::async_trait]
pub(crate) trait AdmissionSink: Send + Sync {
    async fn send(
        &self,
        issue: IssueId,
        message: ActorMessage,
    ) -> Result<(), ActorMessage>;
}

#[async_trait::async_trait]
impl AdmissionSink for OrchestratorInbox {
    async fn send(
        &self,
        issue: IssueId,
        message: ActorMessage,
    ) -> Result<(), ActorMessage> {
        OrchestratorInbox::send(self, issue, message).await
    }
}

/// Drive one normalized observation through the pre-admission judge + dedup
/// index, then route the resulting [`ObserveOutcome`] through `sink`. Pure
/// (apart from logging) so the production loop and the integration-test seam
/// share identical routing logic.
///
/// Returns the [`ObserveOutcome`] the dedup index produced so callers (tests)
/// can assert on the decision branch without observing side effects.
pub(crate) async fn route_observation<S>(
    issue: NormalizedIssue,
    judge: &PreAdmissionJudge,
    dedup: &DedupIndex,
    sink: &S,
) -> ObserveOutcome
where
    S: AdmissionSink + ?Sized,
{
    let issue_id = issue.issue.clone();
    let decision = judge.evaluate(&issue);
    let outcome = dedup.observe(issue, decision).await;
    match &outcome {
        ObserveOutcome::LaunchFresh { issue: normalized, mode } => {
            if let Err(returned) = sink
                .send(
                    normalized.issue.clone(),
                    ActorMessage::TrackerAdmit {
                        mode: *mode,
                        repo: None,
                    },
                )
                .await
            {
                warn!(
                    target: "tracker.admission_pipe.actor_inbox_closed",
                    issue = %normalized.issue,
                    "actor inbox closed; dropping {:?}",
                    returned,
                );
            }
        }
        ObserveOutcome::UpdateInPlace => {
            debug!(
                target: "tracker.pre_admission.update_in_place",
                issue = %issue_id,
                "duplicate observation refreshed in-flight snapshot"
            );
        }
        ObserveOutcome::Drop => {
            // The info-severity `tracker.pre_admission.skipped` log already
            // fired inside `judge.evaluate`; surface a debug-severity event
            // so the routing branch is visible without re-emitting the
            // skipped reason at info level.
            debug!(
                target: "tracker.pre_admission.drop",
                issue = %issue_id,
                "pre-admission failed; dropping observation"
            );
        }
        ObserveOutcome::TerminateInFlight { reason } => {
            let message = match reason {
                TerminationReason::AssignmentLost => ActorMessage::TrackerAssignmentLost,
                TerminationReason::RokiReadyRemoved => ActorMessage::TrackerRokiReadyRemoved,
            };
            if let Err(returned) = sink.send(issue_id.clone(), message).await {
                warn!(
                    target: "tracker.admission_pipe.actor_inbox_closed",
                    issue = %issue_id,
                    "actor inbox closed; dropping {:?}",
                    returned,
                );
            }
        }
    }
    outcome
}

/// Spawned task body: loop on the workspace-level `issue_rx` channel, route
/// every observation through [`route_observation`], and exit cleanly on
/// shutdown or sender closure.
async fn admission_pipe(
    mut issue_rx: mpsc::Receiver<NormalizedIssue>,
    judge: Arc<PreAdmissionJudge>,
    dedup: Arc<DedupIndex>,
    inbox: OrchestratorInbox,
    shutdown_signal: ShutdownSignal,
) {
    loop {
        tokio::select! {
            _ = shutdown_signal.wait() => return,
            msg = issue_rx.recv() => {
                let Some(issue) = msg else {
                    // Both webhook + poller dropped their senders; the
                    // workspace-level mpsc is closed and no more
                    // observations can arrive.
                    return;
                };
                route_observation(issue, judge.as_ref(), dedup.as_ref(), &inbox).await;
            }
        }
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
    /// Optional per-issue debug-sink factory composed in `bootstrap` from
    /// `RunArgs.debug` and `Config.debug.dir` (Req 11.6, 11.7). When
    /// `Some`, the production engine adapters' launch contexts attach a
    /// per-issue `PerIssueDebugSink` so stdout / stderr lines are captured
    /// to `<dir>/<issue>.log`.
    debug_sink_factory: Option<Arc<crate::logging::DebugSinkFactory>>,
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
        debug_sink_factory,
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
        debug_sink_factory,
    })
}

/// Compose the optional [`crate::logging::DebugSinkFactory`] from the
/// merged CLI + config view (Req 11.6, 11.7).
///
/// The factory is built when EITHER `--debug` is set OR `[debug].dir` is
/// populated. Resolution order for the directory:
///
/// 1. `[debug].dir` — operator-declared path; honored verbatim.
/// 2. `--debug` without a configured dir — fall back to the per-process
///    session-manager root joined with `debug/`. The session root sits
///    under the platform user cache directory, so the fallback never
///    lands at a privileged location and survives multiple `roki run`
///    invocations on the same workstation.
///
/// When neither slot is set, returns `None` so the engine adapters keep
/// the existing no-capture behavior.
fn compose_debug_sink_factory(
    args: &RunArgs,
    config: &Config,
) -> Option<Arc<crate::logging::DebugSinkFactory>> {
    let dir = match (&config.debug.dir, args.debug) {
        (Some(path), _) => path.clone(),
        (None, true) => SessionManager::new().root().join("debug"),
        (None, false) => return None,
    };
    Some(Arc::new(crate::logging::DebugSinkFactory::new(dir)))
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

/// Await each per-issue actor `JoinHandle` within `window`, aborting the
/// underlying actor task (not a wrapper) on timeout.
///
/// `await_workers_with_window` is wired for "spawned futures the runtime
/// owns"; for orchestrator actors the runtime already holds the
/// `JoinHandle` returned by `Orchestrator::drain_actors`. Calling
/// `handle.abort()` on those handles is what actually cancels the actor
/// task — wrapping the handle in another spawn would only abort the
/// wrapper and leave the actor running. Mirrors the per-tag completed /
/// timed_out partitioning that `await_workers_with_window` produces so the
/// caller's aggregate reporting stays uniform.
async fn await_actor_handles_with_window(
    handles: Vec<(IssueId, tokio::task::JoinHandle<()>)>,
    window: Duration,
) -> AwaitOutcome {
    if handles.is_empty() {
        return AwaitOutcome::default();
    }
    let mut pending: Vec<(String, tokio::task::JoinHandle<()>)> = handles
        .into_iter()
        .map(|(issue, handle)| (format!("orchestrator-actor:{issue}"), handle))
        .collect();
    let mut outcome = AwaitOutcome::default();
    let sleep = tokio::time::sleep(window);
    tokio::pin!(sleep);

    loop {
        if pending.is_empty() {
            return outcome;
        }
        let next_done = async {
            use std::future::poll_fn;
            use std::pin::Pin;
            use std::task::Poll;
            poll_fn(|cx| {
                for (idx, (_, handle)) in pending.iter_mut().enumerate() {
                    if let Poll::Ready(result) = Pin::new(handle).poll(cx) {
                        return Poll::Ready((result, idx));
                    }
                }
                Poll::Pending
            })
            .await
        };
        tokio::select! {
            (join_result, idx) = next_done => {
                let (tag, _handle) = pending.remove(idx);
                match join_result {
                    Ok(()) => outcome.completed.push(tag),
                    Err(join_err) => {
                        warn!(
                            target: "runtime.shutdown",
                            worker = %tag,
                            error = %join_err,
                            "orchestrator actor join error"
                        );
                        outcome.completed.push(tag);
                    }
                }
            }
            _ = &mut sleep => {
                for (tag, handle) in pending.drain(..) {
                    warn!(
                        target: "runtime.shutdown",
                        worker = %tag,
                        "orchestrator actor exceeded shutdown window; aborting"
                    );
                    handle.abort();
                    outcome.timed_out.push(tag);
                }
                return outcome;
            }
        }
    }
}

/// Sub-window for tracker-side wind-down (phase 1 of the phased shutdown).
/// The tracker poller, webhook server, and admission pipe must all stop
/// accepting new events before the orchestrator actor map is torn down,
/// per design.md "Daemon bootstrap" step 12. The remainder of
/// [`SHUTDOWN_WINDOW`] is reserved for awaiting the actor map (phase 2).
const TRACKER_SHUTDOWN_SUB_WINDOW: Duration = Duration::from_secs(5);

/// Step 12: serve the webhook router and await shutdown.
///
/// Phased wind-down inside [`SHUTDOWN_WINDOW`]:
///
/// 1. **Stop accepting new events.** Wait for the shutdown signal, then
///    await the webhook server, the linear poller, and the admission pipe
///    within [`TRACKER_SHUTDOWN_SUB_WINDOW`] so the orchestrator inbox no
///    longer receives admissions before any actor begins teardown.
/// 2. **Drain the orchestrator actor map.** Drop the [`OrchestratorInbox`]
///    so no future message can be routed in, then call
///    [`Orchestrator::drain_actors`] which drops every per-actor `Sender`.
///    Each actor's `rx.recv()` returns `None` and the actor's loop tail
///    closes the held orchestrator session at the engine seam (stdin close
///    → SIGTERM → bounded wait per the adapter's grace window).
/// 3. **Aggregate timeouts.** A warn-severity log entry surfaces any
///    workers (tracker-side or actor-side) that exceeded the window so the
///    operator log names exactly which subsystem failed to drain.
async fn serve(bootstrap: Bootstrapped) -> Result<(), RuntimeError> {
    let Bootstrapped {
        shutdown_signal,
        listener,
        bind_addr,
        webhook_state,
        issue_rx,
        tracker_handle,
        judge,
        inbox,
        dedup,
        orchestrator,
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

    let server_handle = tokio::spawn(server);
    let admission_handle = tokio::spawn(admission_pipe(
        issue_rx,
        judge,
        dedup,
        inbox.clone(),
        shutdown_signal.clone(),
    ));
    let TrackerHandle {
        refresh: _refresh,
        poller_join,
    } = tracker_handle;

    shutdown_signal.wait().await;

    let shutdown_started = std::time::Instant::now();

    // ---- Phase 1: tracker + webhook + admission pipe stop accepting new
    // events. Bounded by `TRACKER_SHUTDOWN_SUB_WINDOW` so the orchestrator
    // actor map gets the bulk of `SHUTDOWN_WINDOW` for engine-seam shutdown.
    let phase1 = await_workers_with_window(
        [
            ("webhook-server".to_owned(), join_to_unit(server_handle)),
            ("admission-pipe".to_owned(), join_to_unit(admission_handle)),
            ("linear-poller".to_owned(), join_to_unit(poller_join)),
        ],
        TRACKER_SHUTDOWN_SUB_WINDOW,
    )
    .await;

    // ---- Phase 2: drop the OrchestratorInbox + drain per-issue actors.
    // Dropping `inbox` releases the runtime's `Arc<Orchestrator>` clone the
    // admission pipe / recovery seed hold, but the actor map still owns
    // each actor's `Sender`. `drain_actors` removes every entry from the
    // map and returns the join handles; each dropped `Sender` closes the
    // actor's receive side, so the actor's loop tail runs the engine-seam
    // teardown (stdin close + SIGTERM via the adapter's grace).
    drop(inbox);
    let actor_handles = orchestrator.drain_actors();
    let elapsed_phase1 = shutdown_started.elapsed();
    let phase2_window = SHUTDOWN_WINDOW
        .checked_sub(elapsed_phase1)
        .unwrap_or_else(|| Duration::from_secs(0));

    let phase2 = await_actor_handles_with_window(actor_handles, phase2_window).await;

    // ---- Phase 3: aggregate + warn on timeouts.
    let mut combined_timed_out = phase1.timed_out;
    combined_timed_out.extend(phase2.timed_out);
    if !combined_timed_out.is_empty() {
        warn!(
            target: "runtime.shutdown.timed_out",
            timed_out = ?combined_timed_out,
            window_secs = SHUTDOWN_WINDOW.as_secs(),
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
    use std::time::Duration;

    use tokio::sync::Mutex as AsyncMutex;
    use tokio::sync::mpsc;

    use crate::exec::ghq::GhqTool;
    use crate::exec::wt::WtTool;
    use crate::orchestrator::core::{
        Orchestrator, OrchestratorEngine, PhaseEngine, SessionDirOps, WorktreeOps,
    };
    use crate::orchestrator::escalation::EscalationQueue;
    use crate::orchestrator::read::{ActorSnapshot, OrchestratorReadHandle};
    use crate::orchestrator::recovery::{
        DiscoveredIssue, RecoveryDecision, RecoveryReconciler,
    };
    use crate::orchestrator::state::{IssueId, Mode};
    use crate::shutdown::ShutdownSignal;
    use crate::tracker::linear::{BackoffState, LinearClient};
    use crate::tracker::model::{LinearStateName, LinearUserId, NormalizedIssue};
    use crate::tracker::pre_admission::PreAdmissionJudge;
    use crate::tracker::refresh::TrackerRefresh;

    use super::{
        AdmissionSink, Bootstrapped, OrchestratorComposition, OrchestratorInbox,
        OrchestratorSeams, RecoverySeedSink, RuntimeError, bootstrap, compose_orchestrator,
        drive_recovery_seed, route_observation, serve, spawn_workspace_poller,
    };
    use crate::cli::RunArgs;
    use crate::config::EnvReader;
    use crate::shutdown::ShutdownTrigger;
    use crate::orchestrator::core::ActorMessage;
    use crate::orchestrator::tracker_bridge::{DedupIndex, ObserveOutcome};
    use crate::shutdown::AwaitOutcome;

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

    // -----------------------------------------------------------------------
    // Recovery-seed test seam (Task 10.1.3)
    // -----------------------------------------------------------------------

    /// Recording sink used by the recovery-composition integration tests.
    /// Captures every `Admit` the seed routine emits and forwards orphan
    /// retentions onto the supplied `EscalationQueue` so tests can assert on
    /// the exact same observable surface the production sink produces.
    /// One recorded admit observation: `(issue, mode, repo)`. `repo` is
    /// currently always `None` since the recovery seed sends
    /// `TrackerAdmit { repo: None }` and re-resolves the worktree on the
    /// orchestrator's first non-classify phase nomination.
    pub type RecordedAdmit = (IssueId, Mode, Option<crate::tracker::model::RepoId>);
    /// Shared recording buffer used by both the public harness handle and
    /// the internal recording sink so tests observe the exact same vec.
    pub type AdmitBuffer = Arc<AsyncMutex<Vec<RecordedAdmit>>>;

    #[derive(Debug)]
    pub struct RecoverySeedHarness {
        /// Every `Admit` the seed produced.
        pub admits: AdmitBuffer,
        /// Escalation queue the orphan branch enqueues into; assertions read
        /// via `EscalationQueue::snapshot()`.
        pub escalations: Arc<EscalationQueue>,
        /// Issues skipped via the `NoOp` branch.
        pub noops: Arc<AsyncMutex<Vec<IssueId>>>,
    }

    struct RecordingSink {
        admits: AdmitBuffer,
        escalations: Arc<EscalationQueue>,
        noops: Arc<AsyncMutex<Vec<IssueId>>>,
    }

    #[async_trait::async_trait]
    impl RecoverySeedSink for RecordingSink {
        async fn admit(&self, issue: IssueId, mode: Mode) {
            self.admits.lock().await.push((issue, mode, None));
        }

        async fn orphan(&self, issue: IssueId, discovered: &DiscoveredIssue) {
            // Mirror the production sink's escalation shape so test
            // assertions exercise the same structured fields.
            let fields = RecoveryReconciler::<
                crate::exec::wt::MockWt,
                crate::exec::ghq::MockGhq,
            >::orphan_directive_fields(discovered);
            let entry = crate::orchestrator::escalation::EscalationEntry {
                issue: issue.clone(),
                repo: discovered
                    .worktrees
                    .first()
                    .map(|w| w.repo_id.0.clone()),
                kind: crate::orchestrator::escalation::EscalationKind::Orphan,
                correlation_id: format!("recovery-test-{issue}"),
                timestamp: time::OffsetDateTime::now_utc(),
                structured_fields: fields,
            };
            self.escalations.enqueue(entry).await;
        }

        async fn noop(&self, issue: IssueId) {
            self.noops.lock().await.push(issue);
        }
    }

    /// Drive the same recovery scan + decide composition the production
    /// `bootstrap` step performs, but record observations into in-memory
    /// buffers instead of mutating an orchestrator actor map. Mirrors the
    /// production code path so the integration tests exercise the same
    /// `scan` + `decide` + dispatch pipeline.
    pub async fn compose_recovery_for_test<W, G>(
        reconciler: RecoveryReconciler<W, G>,
        linear: Arc<LinearClient>,
        judge: PreAdmissionJudge,
        window: Duration,
    ) -> Result<RecoverySeedHarness, RuntimeError>
    where
        W: WtTool,
        G: GhqTool,
    {
        compose_recovery_for_test_with_extras(reconciler, linear, judge, window, Vec::new()).await
    }

    // -----------------------------------------------------------------------
    // Tracker-poller test seam (Task 10.1.4)
    // -----------------------------------------------------------------------

    /// Harness returned by [`compose_poller_for_test`]. Mirrors the
    /// runtime-internal `TrackerHandle` shape but exposes the underlying
    /// `BackoffState` so integration tests can assert on the deadline curve
    /// without re-issuing requests.
    pub struct PollerHarness {
        /// Cheap-clone refresh handle. Kept as `Arc<dyn TrackerRefresh>` so
        /// tests exercise the same trait surface 10.1.5 / 10.1.6 will rely on.
        pub refresh: Arc<dyn TrackerRefresh>,
        /// Spawned poller's join handle. Tests await this within a bounded
        /// window after firing shutdown to assert clean exit.
        pub poller_join: tokio::task::JoinHandle<()>,
        /// Shared backoff state — the same `Arc<BackoffState>` the poller
        /// observes. Tests use it to (a) observe the deadline shift after a
        /// 429 lands, and (b) force the deadline forward via
        /// [`BackoffState::set_deadline_for_test`].
        pub backoff: Arc<BackoffState>,
    }

    /// Compose the same workspace-level poller + `TrackerRefresh` handle the
    /// production `bootstrap` step constructs, but accept a caller-supplied
    /// `LinearClient` (typically pointed at a wiremock URI with a shrunk
    /// backoff curve via
    /// [`crate::tracker::linear::LinearClient::with_backoff_window`]) and a
    /// caller-controlled `ShutdownSignal`. The cadence floor argument is the
    /// same `Duration` the production path reads from
    /// `[linear].poll_cadence_seconds`; tests pass sub-second values to keep
    /// runtime bounded without hitting the production `MIN_POLL_CADENCE_SECONDS`
    /// floor (which is enforced at config load, not at this layer).
    pub fn compose_poller_for_test(
        client: LinearClient,
        assignee: LinearUserId,
        states: Vec<LinearStateName>,
        cadence_floor: Duration,
        sink: mpsc::Sender<NormalizedIssue>,
        shutdown_signal: ShutdownSignal,
    ) -> Result<PollerHarness, RuntimeError> {
        let backoff = client.backoff();
        let handle = spawn_workspace_poller(
            client,
            assignee,
            states,
            cadence_floor,
            sink,
            shutdown_signal,
        );
        Ok(PollerHarness {
            refresh: handle.refresh,
            poller_join: handle.poller_join,
            backoff,
        })
    }

    /// Variant accepting synthetic decisions appended after the on-disk
    /// scan. Lets tests exercise the `FreshQueued` / `NoOp` cells without
    /// having to coax the on-disk scan into emitting them (the scan only
    /// emits issues observable on disk).
    pub async fn compose_recovery_for_test_with_extras<W, G>(
        reconciler: RecoveryReconciler<W, G>,
        linear: Arc<LinearClient>,
        judge: PreAdmissionJudge,
        window: Duration,
        extras: Vec<RecoveryDecision>,
    ) -> Result<RecoverySeedHarness, RuntimeError>
    where
        W: WtTool,
        G: GhqTool,
    {
        let admits = Arc::new(AsyncMutex::new(Vec::new()));
        let escalations = Arc::new(EscalationQueue::new());
        let noops = Arc::new(AsyncMutex::new(Vec::new()));
        let sink = RecordingSink {
            admits: admits.clone(),
            escalations: escalations.clone(),
            noops: noops.clone(),
        };
        drive_recovery_seed(&reconciler, &linear, &judge, &sink, window, extras).await?;
        Ok(RecoverySeedHarness {
            admits,
            escalations,
            noops,
        })
    }

    // -----------------------------------------------------------------------
    // Admission-pipe test seam (Task 10.1.5)
    // -----------------------------------------------------------------------

    /// One recorded inbox observation: `(issue, message)`. The message is
    /// stored as a typed `ActorMessage` so tests can pattern-match the
    /// routed variant without parsing log output.
    pub type RecordedInboxMessage = (IssueId, ActorMessage);

    /// Recording inbox used by the admission-pipe integration tests. Captures
    /// every `ActorMessage` the pipe routes for an issue without spawning a
    /// per-issue actor, so tests can assert on the routed-message sequence
    /// (per-actor side effects are exercised by the orchestrator core's own
    /// integration tests).
    #[derive(Debug, Default)]
    pub struct RecordingInbox {
        messages: AsyncMutex<Vec<RecordedInboxMessage>>,
    }

    impl RecordingInbox {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// Snapshot of every routed message so far.
        pub async fn snapshot(&self) -> Vec<RecordedInboxMessage> {
            self.messages
                .lock()
                .await
                .iter()
                .map(|(id, msg)| (id.clone(), clone_actor_message(msg)))
                .collect()
        }

        /// Count the number of `TrackerAdmit` messages routed for `issue`.
        pub async fn admit_count_for(&self, issue: &IssueId) -> usize {
            self.messages
                .lock()
                .await
                .iter()
                .filter(|(id, msg)| {
                    id == issue && matches!(msg, ActorMessage::TrackerAdmit { .. })
                })
                .count()
        }
    }

    fn clone_actor_message(message: &ActorMessage) -> ActorMessage {
        // `ActorMessage` deliberately does not implement Clone (variants
        // carry per-message payloads that are not safely duplicable). The
        // recording inbox only stores the variants the admission pipe can
        // route — `TrackerAdmit` / `TrackerAssignmentLost` /
        // `TrackerRokiReadyRemoved` — so we can hand-roll a Clone for those.
        match message {
            ActorMessage::TrackerAdmit { mode, repo } => {
                ActorMessage::TrackerAdmit {
                    mode: *mode,
                    repo: repo.clone(),
                }
            }
            ActorMessage::TrackerAssignmentLost => ActorMessage::TrackerAssignmentLost,
            ActorMessage::TrackerRokiReadyRemoved => ActorMessage::TrackerRokiReadyRemoved,
            // The admission pipe never routes the remaining variants; if
            // a test extension surfaces one, surface a panic so the broken
            // assumption is loud rather than silently mis-recorded.
            other => panic!(
                "RecordingInbox does not record non-admission ActorMessage variant: {other:?}"
            ),
        }
    }

    #[async_trait::async_trait]
    impl AdmissionSink for RecordingInbox {
        async fn send(
            &self,
            issue: IssueId,
            message: ActorMessage,
        ) -> Result<(), ActorMessage> {
            self.messages.lock().await.push((issue, message));
            Ok(())
        }
    }

    /// Compose-and-drive a single admission-pipe observation through the
    /// production routing logic. Returns the `ObserveOutcome` so tests can
    /// assert on the dedup-decision branch without observing the inbox.
    pub async fn drive_admission_for_test(
        issue: NormalizedIssue,
        judge: &PreAdmissionJudge,
        dedup: &DedupIndex,
        sink: &RecordingInbox,
    ) -> ObserveOutcome {
        route_observation(issue, judge, dedup, sink).await
    }

    // -----------------------------------------------------------------------
    // Phased-shutdown test seam (Task 10.1.6)
    // -----------------------------------------------------------------------

    /// Drive phase 2 of the daemon's phased shutdown in isolation: drop the
    /// supplied `OrchestratorInbox` so no admission pipe / recovery sink can
    /// route a fresh message in, then call `Orchestrator::drain_actors` and
    /// await each per-issue actor's `JoinHandle` within `window` via the
    /// production `await_workers_with_window`. Mirrors the production
    /// `serve()` phase-2 ordering so integration tests exercise the same
    /// teardown shape — drop inbox, drain map, await within window — without
    /// standing up the webhook server / poller / admission pipe.
    pub async fn drain_actors_with_window_for_test(
        orchestrator: Arc<Orchestrator>,
        inbox: OrchestratorInbox,
        window: Duration,
    ) -> AwaitOutcome {
        drop(inbox);
        let handles = orchestrator.drain_actors();
        super::await_actor_handles_with_window(handles, window).await
    }

    // -----------------------------------------------------------------------
    // Shutdown-trigger test seam (Task 10.5)
    // -----------------------------------------------------------------------

    /// Opaque wrapper around the runtime-private `Bootstrapped` so the
    /// test seam can hand a fully composed runtime back to integration
    /// tests without exposing the struct's field shape. The wrapper
    /// only forwards to the production `serve()` step, preserving
    /// production semantics in test composition.
    pub struct BootstrappedForTest {
        inner: Bootstrapped,
    }

    impl BootstrappedForTest {
        /// Drive the production `serve()` step against the wrapped
        /// `Bootstrapped`. Returns `Ok(())` once shutdown completes
        /// inside `SHUTDOWN_WINDOW`, exactly mirroring `run_with_env`'s
        /// post-bootstrap behavior — but without any OS signal handler
        /// installed, so the only path to shutdown is firing the
        /// `ShutdownTrigger` returned alongside this wrapper from
        /// [`bootstrap_for_test`].
        pub async fn serve(self) -> Result<(), RuntimeError> {
            serve(self.inner).await
        }

        /// Test-only accessor: did bootstrap compose a per-issue
        /// [`crate::logging::DebugSinkFactory`] from the merged
        /// `RunArgs.debug` + `[debug].dir` view (Req 11.6, 11.7)?
        ///
        /// e2e_bootstrap test (d) asserts on this rather than driving a
        /// full webhook + orchestrator-session round trip, because the
        /// engine-side file-write contract is already proven by
        /// `engine_impl::launch_with_debug_sink_factory_writes_per_issue_log_file`
        /// (Task 10.6). This accessor closes the runtime-composition gap:
        /// it proves that when `--debug` (or `[debug].dir`) is set, the
        /// production bootstrap composition path actually wires a factory
        /// onto `RuntimeComponents`.
        pub fn has_debug_sink_factory(&self) -> bool {
            self.inner.components.debug_sink_factory.is_some()
        }

        /// Test-only accessor: did bootstrap wire the production
        /// [`crate::engine::phase_subprocess::engine_impl::PhaseSubprocessEngineImpl`]
        /// into the orchestrator actor map (as opposed to the placeholder
        /// that previously refused every `run_phase` call)?
        ///
        /// The accessor inspects the strong count of the
        /// `phase_subprocess_adapter` Arc held on `RuntimeComponents`: when
        /// the production composition path runs, the engine impl clones the
        /// adapter into its own slot, lifting the count to >= 2. The
        /// previous placeholder did not retain the adapter, so an
        /// integration test can use this signal to assert the real wiring.
        pub fn has_real_phase_engine(&self) -> bool {
            std::sync::Arc::strong_count(&self.inner.components.phase_subprocess_adapter) >= 2
        }
    }

    /// Compose the runtime via the production `bootstrap` step but skip
    /// the OS signal handler install. Returns the composed runtime
    /// (wrapped in [`BootstrappedForTest`]) plus the matching
    /// [`ShutdownTrigger`] so e2e tests can fire shutdown directly on
    /// the trigger instead of delivering SIGINT/SIGTERM to the test
    /// harness process.
    ///
    /// Production code paths must continue to use [`super::run`] /
    /// [`super::run_with_env`], which install the signal handlers via
    /// [`super::install_signal_handlers`] before driving `serve()`.
    pub async fn bootstrap_for_test(
        args: RunArgs,
        env: &dyn EnvReader,
    ) -> Result<(BootstrappedForTest, ShutdownTrigger), RuntimeError> {
        let (inner, trigger) = bootstrap(args, env).await?;
        Ok((BootstrappedForTest { inner }, trigger))
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
            debug_sink_factory: None,
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

    // ----- Debug sink factory composition (Task 10.6) -----

    fn config_with_debug_dir(dir: Option<&Path>) -> Config {
        let workflow_dir = tempfile::TempDir::new().unwrap();
        let workflow = workflow_dir.path().join("WORKFLOW.md");
        std::fs::write(&workflow, "stub").unwrap();
        let mut body = format!(
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
        if let Some(path) = dir {
            body.push_str(&format!("\n[debug]\ndir = \"{}\"\n", path.display()));
        }
        // Anchor the workflow tempdir for the test caller — we leak the
        // dir handle deliberately so the workflow file survives the
        // `Config::load_from_str` call without the caller managing
        // tempfiles. The test process exits shortly after either way.
        std::mem::forget(workflow_dir);
        Config::load_from_str(&body).unwrap()
    }

    fn args_with_debug(debug: bool) -> RunArgs {
        RunArgs {
            config: None,
            bind: None,
            port: None,
            dangerously_skip_permissions: false,
            debug,
        }
    }

    #[test]
    fn compose_debug_sink_factory_returns_none_when_neither_flag_nor_dir_set() {
        let cfg = config_with_debug_dir(None);
        let factory = compose_debug_sink_factory(&args_with_debug(false), &cfg);
        assert!(factory.is_none());
    }

    #[test]
    fn compose_debug_sink_factory_uses_config_dir_when_present() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = config_with_debug_dir(Some(dir.path()));
        let factory = compose_debug_sink_factory(&args_with_debug(false), &cfg)
            .expect("factory built when [debug].dir is set");

        // Materialize a sink and write a line to confirm the factory points
        // at the configured directory.
        let mut sink = factory.for_issue("ENG-42");
        sink.append(
            crate::logging::StreamTag::Stdout,
            &crate::logging::RoleTag::Orchestrator,
            "wired",
        );
        let target = dir.path().join("ENG-42.log");
        assert!(
            target.exists(),
            "factory must point at <[debug].dir>/<issue>.log; got {target:?}"
        );
    }

    #[test]
    fn compose_debug_sink_factory_falls_back_to_session_root_when_only_flag_set() {
        let cfg = config_with_debug_dir(None);
        let factory = compose_debug_sink_factory(&args_with_debug(true), &cfg)
            .expect("factory built when --debug is set");

        // Verify the factory writes under the documented fallback root.
        let session_root = SessionManager::new().root().to_path_buf();
        let mut sink = factory.for_issue("ENG-fallback");
        sink.append(
            crate::logging::StreamTag::Stdout,
            &crate::logging::RoleTag::Orchestrator,
            "wired",
        );
        let target = session_root.join("debug").join("ENG-fallback.log");
        assert!(
            target.exists(),
            "fallback must land under <session_root>/debug/<issue>.log; got {target:?}"
        );
        // Cleanup so subsequent runs do not see a stale file.
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn compose_debug_sink_factory_prefers_config_dir_over_flag_fallback() {
        // If both `[debug].dir` and `--debug` are set, the operator-declared
        // directory wins so an explicitly-mounted volume is honored.
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = config_with_debug_dir(Some(dir.path()));
        let factory = compose_debug_sink_factory(&args_with_debug(true), &cfg)
            .expect("factory built when both flag and dir are set");

        let mut sink = factory.for_issue("ENG-both");
        sink.append(
            crate::logging::StreamTag::Stdout,
            &crate::logging::RoleTag::Orchestrator,
            "wired",
        );
        assert!(dir.path().join("ENG-both.log").exists());
    }

    #[test]
    fn assemble_runtime_components_threads_debug_sink_factory() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut inputs = fixture_inputs(dir.path());
        let debug_dir = tempfile::TempDir::new().unwrap();
        let factory = Arc::new(crate::logging::DebugSinkFactory::new(debug_dir.path()));
        inputs.debug_sink_factory = Some(factory.clone());
        let components = assemble_runtime_components(inputs).expect("assembly succeeds");
        let attached = components
            .debug_sink_factory
            .expect("RuntimeComponents must carry the factory the bootstrap supplied");
        assert!(Arc::ptr_eq(&attached, &factory));
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

