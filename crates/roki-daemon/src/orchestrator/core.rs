//! Orchestrator runtime: per-issue worker actor and the supervising event
//! loop.
//!
//! Task 3.2 of the roki-mvp spec, with the per-task-7.1b key collapse from
//! `(repo, issue)` to `(issue,)` applied. This module owns the central
//! [`Orchestrator`] struct that:
//!
//! * holds the canonical in-memory state map (`Arc<RwLock<HashMap<IssueId,
//!   ActorRecord>>>`);
//! * spawns one tokio task per `IssueId` — the per-issue worker actor;
//! * routes [`NormalizedIssue`] events from the tracker inbox to the right
//!   actor;
//! * gates every committed transition through [`EventBus::publish`] (which
//!   handles the vetoable-subscriber contract for the three vetoable
//!   transitions);
//! * gates the `TerminalSuccess -> Cleaning` transition through both the
//!   subscriber chain and the [`HookRegistry`] pre-cleanup hooks (a `Deny`
//!   from either side stays in `TerminalSuccess`);
//! * shuts down cooperatively when [`ShutdownSignal::wait`] resolves.
//!
//! ## What this module does NOT do (post-7.1b)
//!
//! Workspace lifecycle wiring (session-tempdir creation on `Queued -> Active`
//! and worktree teardown on `Cleaning`) is replaced with NoOp shims pending
//! task 7.1d (`SessionManager` + `WorktreeRegistry`). The NoOp shims keep
//! the actor advancing through the lifecycle so unit tests for the
//! retry-budget loop, vetoable-transition gating, and pre-cleanup hooks all
//! exercise the same arcs they did pre-7.1b. Anything that touched real
//! `wt`/`ghq` plumbing has been pulled out and tagged `// TODO(7.1d):`.
//!
//! ## Boundary
//!
//! The orchestrator depends on a small [`EngineLauncher`] trait rather than a
//! concrete adapter so the integration test in `tests/orchestrator_core.rs`
//! can stub engine launches without spawning real subprocesses. The
//! `workspace: Arc<dyn Workspace>` field is retained as a placeholder for
//! restart recovery's `list_existing` call (see [`Orchestrator::with_recovery`]);
//! the worker actor itself no longer calls `ensure`/`remove`. Task 7.1d
//! drops the `Workspace` trait dependency entirely.
//!
//! [`ClaudeEngineAdapter::launch`] already matches the [`EngineLauncher`]
//! signature so a wrapper impl can be added without breaking core.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::engine::policy::{EnginePolicy, WorkerOutcome};
use crate::engine::{SupervisedEvent, WorkerContext};
use crate::orchestrator::events::EventBus;
use crate::orchestrator::hooks::{HookRegistry, PreCleanupContext};
use crate::orchestrator::read::{IssueState, OrchestratorRead, SnapshotResponse};
use crate::orchestrator::recovery::{
    RecoveryDecision, RecoveryError, RecoveryLinearReader, run_recovery,
};
use crate::orchestrator::state::{
    CorrelationId, IssueId, RepoId, TransitionEvent, TransitionTrigger, VetoDecision, WorkerState,
};
use crate::permissions::{PermissionMode, PermissionSource, ResolvedPermission};
use crate::shutdown::ShutdownSignal;
use crate::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use crate::workflow::{ElicitationsMode, SandboxMode};
use crate::workspace::Workspace;

/// Error type re-exported for the engine launcher trait so downstream test
/// stubs and the real `ClaudeEngineAdapter` can both surface launch failures
/// through the same shape. Mirrors [`crate::engine::LaunchError`] but lives
/// here so the orchestrator core does not couple every consumer of the trait
/// to the adapter's full surface.
#[derive(Debug, Error)]
pub enum LaunchError {
    /// Underlying engine adapter failed before any lifecycle events were
    /// produced.
    #[error("engine launch failed: {0}")]
    Engine(String),
}

/// Engine adapter abstraction consumed by the orchestrator core.
///
/// The contract matches the existing [`crate::engine::ClaudeEngineAdapter`]:
/// every successful launch emits exactly one terminal
/// [`SupervisedEvent::Exited`] event before resolving with the same
/// [`WorkerOutcome`]. Implementations must be `Send + Sync + 'static` so they
/// can be held in an `Arc<dyn EngineLauncher>` across the per-actor task.
#[async_trait]
pub trait EngineLauncher: Send + Sync + 'static {
    /// Launch a supervised worker session and stream events into `events`.
    async fn launch(
        &self,
        ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError>;
}

/// Internal record kept in the orchestrator state map for each tracked
/// issue. Used by the `OrchestratorRead` projection.
#[derive(Debug, Clone)]
struct ActorRecord {
    state: WorkerState,
    last_event_at: Option<SystemTime>,
    last_correlation_id: Option<CorrelationId>,
    /// Sender into the per-actor inbox, used by the orchestrator runtime to
    /// forward tracker events. `None` once the actor has terminated.
    inbox: Option<mpsc::Sender<ActorCommand>>,
    /// Number of consecutive `NonCleanExit` outcomes recorded for this
    /// issue since the actor entered `Active`. Drives the retry budget
    /// enforced in [`WorkerActor::try_promote_to_active`] (task 3.7,
    /// SPEC.md §9.5).
    ///
    /// The state machine forbids re-entering `Active` after `AwaitingReview`
    /// (no `AwaitingReview → Queued` arc), so reset is unreachable; the
    /// counter is monotonic for the actor's lifetime and intentionally has no
    /// reset path. Documented invariant — do not write dead reset code.
    consecutive_failures: u32,
}

/// Commands routed into a per-issue actor's inbox.
///
/// Lifecycle events from the engine are NOT carried through this enum —
/// each actor owns its own engine-events `mpsc::Receiver` and consumes them
/// directly from the supervised channel returned by the engine adapter.
#[derive(Debug)]
enum ActorCommand {
    /// Tracker observed the issue in a fresh state (or unchanged).
    Tracker(NormalizedIssue),
    /// Operator shutdown — actor must wind down.
    Shutdown,
}

/// Read projection backed by the orchestrator's live state map.
///
/// Cheap to clone — wraps the same `Arc<RwLock<...>>` the orchestrator uses.
/// Implements [`OrchestratorRead`] in a strictly read-only fashion: there is
/// no `&mut self` method anywhere on this surface.
#[derive(Clone)]
pub struct OrchestratorReadHandle {
    state: Arc<RwLock<HashMap<IssueId, ActorRecord>>>,
}

impl OrchestratorRead for OrchestratorReadHandle {
    fn snapshot(&self) -> SnapshotResponse {
        let guard = self
            .state
            .read()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        let issues: Vec<IssueState> = guard
            .iter()
            .map(|(issue, record)| IssueState {
                issue: issue.clone(),
                state: record.state,
                last_event_at: record.last_event_at,
                last_correlation_id: record.last_correlation_id,
            })
            .collect();
        SnapshotResponse::new(issues)
    }

    fn issue(&self, issue: &IssueId) -> Option<IssueState> {
        let guard = self
            .state
            .read()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        guard.get(issue).map(|record| IssueState {
            issue: issue.clone(),
            state: record.state,
            last_event_at: record.last_event_at,
            last_correlation_id: record.last_correlation_id,
        })
    }
}

/// Central orchestrator runtime.
///
/// One [`Orchestrator`] instance per daemon. Owns the canonical state map
/// and the per-actor task supervisors. Construct with [`Orchestrator::new`],
/// call [`Orchestrator::read_handle`] to grab a read-only projection handle
/// before starting, then drive [`Orchestrator::run`] from a tokio task. The
/// `run` future resolves only after [`ShutdownSignal::wait`] resolves.
pub struct Orchestrator {
    /// Workspace adapter retained for restart-recovery's `list_existing`
    /// call (see [`Self::with_recovery`]). The worker actor no longer calls
    /// `ensure`/`remove` post-7.1b — those become NoOp shims until 7.1d
    /// wires `SessionManager` and `WorktreeRegistry`. The trait dependency
    /// itself drops in 7.1d. The field is currently set-only on the
    /// orchestrator runtime path; `with_recovery` consumes the same value
    /// before constructing `Self`, so reading it back here would be
    /// redundant.
    #[allow(
        dead_code,
        reason = "Retained for the 7.1d wiring (drops the trait dependency entirely) so the public constructor signature stays stable across the 7.1b → 7.1d transition."
    )]
    workspace: Arc<dyn Workspace>,
    engine: Arc<dyn EngineLauncher>,
    event_bus: Arc<EventBus>,
    hook_registry: Arc<HookRegistry>,
    shutdown: ShutdownSignal,
    tracker_inbox: mpsc::Receiver<NormalizedIssue>,
    state: Arc<RwLock<HashMap<IssueId, ActorRecord>>>,
    /// Issues whose workspace lifecycle hit a hard fault (creation or deletion
    /// error). Per Requirement 4.5, the orchestrator refuses to start
    /// additional work for a poisoned issue until an operator intervenes.
    /// This set is append-only for the daemon's lifetime; restart recovery
    /// (task 3.3 / 7.1e) is the operator's intervention surface.
    ///
    /// Post-7.1b the workspace lifecycle is NoOp-stubbed so this set is
    /// currently never populated by production paths; the surface is kept
    /// so 7.1d can repopulate it from the new `SessionManager` /
    /// `WorktreeRegistry` failure paths without rewiring the dispatch
    /// guard.
    poisoned: Arc<RwLock<HashSet<IssueId>>>,
    /// Per-orchestrator [`EnginePolicy`] used to construct each actor's
    /// `WorkerContext` and to drive the retry-budget Backoff loop in
    /// [`WorkerActor::try_promote_to_active`]. Defaults to
    /// [`EnginePolicy::default`]; tests override via
    /// [`Orchestrator::with_engine_policy`] to drop the backoff floor into
    /// the millisecond range so retry traces complete deterministically in
    /// well under one second (task 3.7).
    engine_policy: EnginePolicy,
}

impl Orchestrator {
    /// Construct a new orchestrator. The caller is expected to inject the
    /// canonical singletons (workspace, engine, event bus, hook registry,
    /// shutdown signal) and a `tracker_inbox` receiver fed by the
    /// tracker→orchestrator bridge (task 3.6).
    ///
    /// The `workspace` argument is currently retained only for
    /// [`Self::with_recovery`]; the worker actor itself no longer calls
    /// into it (post-7.1b). Task 7.1d drops the parameter entirely.
    pub fn new(
        workspace: Arc<dyn Workspace>,
        engine: Arc<dyn EngineLauncher>,
        event_bus: Arc<EventBus>,
        hook_registry: Arc<HookRegistry>,
        shutdown: ShutdownSignal,
        tracker_inbox: mpsc::Receiver<NormalizedIssue>,
    ) -> Self {
        Self {
            workspace,
            engine,
            event_bus,
            hook_registry,
            shutdown,
            tracker_inbox,
            state: Arc::new(RwLock::new(HashMap::new())),
            poisoned: Arc::new(RwLock::new(HashSet::new())),
            engine_policy: EnginePolicy::default(),
        }
    }

    /// Replace the per-orchestrator [`EnginePolicy`] with `policy`.
    ///
    /// The default policy uses [`crate::engine::policy::BACKOFF_FLOOR`] (10s)
    /// and `max_attempts = 3`. Production callers normally accept the default;
    /// tests pass a sub-second `backoff_floor` so retry traces complete
    /// deterministically. Future work may resolve this from `WORKFLOW.md`.
    #[must_use]
    pub fn with_engine_policy(mut self, policy: EnginePolicy) -> Self {
        self.engine_policy = policy;
        self
    }

    /// Construct an orchestrator after running the restart-recovery scan
    /// (task 3.3; rewritten in 7.1e).
    ///
    /// This async constructor performs the documented per-issue
    /// reconciliation across the workspace root and Linear before returning.
    /// Synthetic active-state tracker events are posted into
    /// `recovery_sender` for `ResumeActive` and `FreshQueued` decisions so
    /// the orchestrator's existing tracker-event path drives each issue back
    /// into the active lifecycle once [`Orchestrator::run`] starts.
    /// `recovery_sender` is the same `mpsc::Sender<NormalizedIssue>` that
    /// feeds `tracker_inbox`; the caller normally owns both ends of the
    /// channel and threads them through here.
    ///
    /// Returns the constructed [`Orchestrator`] together with the ordered
    /// list of [`RecoveryDecision`] outcomes so callers can log the
    /// reconciliation summary or, in tests, assert per-key post-recovery
    /// state.
    #[allow(
        clippy::too_many_arguments,
        reason = "Orchestrator wiring requires every singleton plus the recovery sender and reader; collapsing into a builder would obscure the constructor's contract for the single caller (the daemon main)."
    )]
    pub async fn with_recovery(
        workspace: Arc<dyn Workspace>,
        engine: Arc<dyn EngineLauncher>,
        event_bus: Arc<EventBus>,
        hook_registry: Arc<HookRegistry>,
        shutdown: ShutdownSignal,
        tracker_inbox: mpsc::Receiver<NormalizedIssue>,
        recovery_sender: mpsc::Sender<NormalizedIssue>,
        reader: &dyn RecoveryLinearReader,
    ) -> Result<(Self, Vec<RecoveryDecision>), RecoveryError> {
        let decisions = run_recovery(workspace.as_ref(), reader, &recovery_sender).await?;
        let orchestrator = Self::new(
            workspace,
            engine,
            event_bus,
            hook_registry,
            shutdown,
            tracker_inbox,
        );
        Ok((orchestrator, decisions))
    }

    /// Return a cheap-to-clone read-only handle into the orchestrator state
    /// map. The handle implements [`OrchestratorRead`] and grants no
    /// mutation rights.
    pub fn read_handle(&self) -> OrchestratorReadHandle {
        OrchestratorReadHandle {
            state: Arc::clone(&self.state),
        }
    }

    /// Run the orchestrator until shutdown. The future resolves once the
    /// shutdown signal fires and every spawned actor has either exited or
    /// been signalled to wind down.
    pub async fn run(mut self) {
        info!(target: "orchestrator", "orchestrator runtime started");

        loop {
            tokio::select! {
                biased;
                () = self.shutdown.wait() => {
                    debug!(target: "orchestrator", "shutdown signal observed; stopping inbox drain");
                    break;
                }
                maybe_issue = self.tracker_inbox.recv() => {
                    match maybe_issue {
                        Some(issue) => {
                            self.dispatch_tracker_event(issue).await;
                        }
                        None => {
                            // Tracker inbox closed; drop into shutdown
                            // wait so we still terminate cleanly when
                            // the shutdown signal fires.
                            debug!(target: "orchestrator", "tracker inbox closed");
                            self.shutdown.wait().await;
                            break;
                        }
                    }
                }
            }
        }

        // Signal every actor to wind down.
        let inboxes: Vec<mpsc::Sender<ActorCommand>> = {
            let guard = self
                .state
                .read()
                .expect("orchestrator state RwLock poisoned; this is unrecoverable");
            guard
                .values()
                .filter_map(|record| record.inbox.clone())
                .collect()
        };
        for inbox in inboxes {
            let _ = inbox.send(ActorCommand::Shutdown).await;
        }
        info!(target: "orchestrator", "orchestrator runtime stopped");
    }

    /// Forward a tracker event to the right actor, spawning a fresh actor if
    /// this is the first time we see the issue.
    ///
    /// Note: post-7.1b `NormalizedIssue.repo` is intentionally ignored here.
    /// The state-machine key is the issue alone; repo association moves onto
    /// the (post-7.1d) `WorktreeRegistry` per worktree the agent opens.
    async fn dispatch_tracker_event(&mut self, issue: NormalizedIssue) {
        let key = issue.issue.clone();

        // Refuse any further work for a poisoned issue: a workspace
        // creation or deletion error already drove this issue into a fault
        // state and the operator must intervene before we resume. We log +
        // skip here so the orchestrator never silently ignores tracker
        // events. Requirement 4.5.
        {
            let guard = self
                .poisoned
                .read()
                .expect("orchestrator poisoned-set RwLock poisoned; this is unrecoverable");
            if guard.contains(&key) {
                warn!(
                    target: "orchestrator",
                    issue = %key.as_str(),
                    "tracker event refused for poisoned issue; operator intervention required",
                );
                return;
            }
        }

        let inbox = {
            let mut guard = self
                .state
                .write()
                .expect("orchestrator state RwLock poisoned; this is unrecoverable");
            match guard.get(&key) {
                Some(record) if record.inbox.is_some() => {
                    record.inbox.clone().expect("inbox is Some by guard above")
                }
                _ => {
                    // Fresh actor.
                    let (tx, rx) = mpsc::channel::<ActorCommand>(32);
                    let record = ActorRecord {
                        state: WorkerState::Discovered,
                        last_event_at: None,
                        last_correlation_id: None,
                        inbox: Some(tx.clone()),
                        consecutive_failures: 0,
                    };
                    guard.insert(key.clone(), record);
                    drop(guard);
                    self.spawn_actor(key.clone(), rx);
                    tx
                }
            }
        };
        if inbox.send(ActorCommand::Tracker(issue)).await.is_err() {
            warn!(
                target: "orchestrator",
                issue = %key.as_str(),
                "actor inbox closed before tracker event could be delivered",
            );
        }
    }

    /// Spawn the per-issue actor task.
    fn spawn_actor(&self, key: IssueId, rx: mpsc::Receiver<ActorCommand>) {
        let actor = WorkerActor {
            key,
            state: Arc::clone(&self.state),
            engine: Arc::clone(&self.engine),
            event_bus: Arc::clone(&self.event_bus),
            hook_registry: Arc::clone(&self.hook_registry),
            poisoned: Arc::clone(&self.poisoned),
            engine_policy: self.engine_policy,
            shutdown: self.shutdown.clone(),
        };
        tokio::spawn(async move { actor.run(rx).await });
    }
}

/// Per-issue worker actor.
///
/// Owns one issue, drives the state machine `Discovered -> Queued -> Active
/// -> AwaitingReview -> TerminalSuccess -> Cleaning -> [*]` through tracker
/// and engine events. Every committed transition is published through
/// [`EventBus::publish`]; the three vetoable transitions are gated through
/// the bus's vetoable path (and the `TerminalSuccess -> Cleaning` transition
/// also through the [`HookRegistry`]).
struct WorkerActor {
    key: IssueId,
    state: Arc<RwLock<HashMap<IssueId, ActorRecord>>>,
    engine: Arc<dyn EngineLauncher>,
    event_bus: Arc<EventBus>,
    hook_registry: Arc<HookRegistry>,
    /// Shared with [`Orchestrator`] so a workspace fault recorded inside the
    /// actor immediately fences off subsequent tracker events for the same
    /// issue. Requirement 4.5. Currently unused on the production path
    /// (post-7.1b NoOp shims); 7.1d repopulates this from the new
    /// `SessionManager` / `WorktreeRegistry` failure paths.
    #[allow(
        dead_code,
        reason = "Surface retained for the 7.1d wiring; the NoOp shims used post-7.1b never fault, so the field is currently set-only. Removing it now would require re-introducing the field in 7.1d."
    )]
    poisoned: Arc<RwLock<HashSet<IssueId>>>,
    /// Per-launch policy carried from the orchestrator. Drives the
    /// retry-budget Backoff loop (`max_attempts`, `backoff_floor`, etc.) and
    /// is also forwarded into the [`WorkerContext`] passed to each engine
    /// launch so the supervisor uses the same policy bounds.
    engine_policy: EnginePolicy,
    /// Cloned shutdown signal so the actor can abort a Backoff sleep cleanly
    /// when the orchestrator is asked to wind down (Requirement 1.3).
    shutdown: ShutdownSignal,
}

impl WorkerActor {
    async fn run(self, mut rx: mpsc::Receiver<ActorCommand>) {
        let correlation = CorrelationId::new();

        loop {
            let current_state = self.read_current_state();
            // Cleaning and TerminalFailure are terminal-ends in the
            // state-machine table; once the actor reaches one of them it
            // unwinds and the task exits.
            if matches!(
                current_state,
                WorkerState::Cleaning | WorkerState::TerminalFailure
            ) {
                debug!(
                    target: "orchestrator",
                    issue = %self.key.as_str(),
                    state = ?current_state,
                    "actor reached terminal end; exiting",
                );
                self.clear_inbox();
                return;
            }

            // Drain a single command. The actor wakes only on tracker
            // events, on shutdown, or when its inbox closes.
            let command = match rx.recv().await {
                Some(cmd) => cmd,
                None => {
                    debug!(
                        target: "orchestrator",
                        issue = %self.key.as_str(),
                        "actor inbox closed; exiting",
                    );
                    return;
                }
            };

            match command {
                ActorCommand::Shutdown => {
                    info!(
                        target: "orchestrator",
                        issue = %self.key.as_str(),
                        "actor received shutdown; exiting",
                    );
                    return;
                }
                ActorCommand::Tracker(issue) => {
                    self.handle_tracker_event(&issue, correlation, &mut rx)
                        .await;
                }
            }
        }
    }

    /// Drive the state machine in response to a tracker event. Multiple
    /// transitions may be performed in sequence (for example `Discovered ->
    /// Queued -> Active`) when the engine session resolves immediately.
    async fn handle_tracker_event(
        &self,
        issue: &NormalizedIssue,
        correlation: CorrelationId,
        rx: &mut mpsc::Receiver<ActorCommand>,
    ) {
        let current = self.read_current_state();

        match (current, issue.state) {
            (WorkerState::Discovered, TrackerIssueState::Active)
            | (WorkerState::Discovered, TrackerIssueState::Review) => {
                if !self
                    .commit_transition(
                        WorkerState::Discovered,
                        WorkerState::Queued,
                        TransitionTrigger::TrackerEvent,
                        correlation,
                    )
                    .await
                {
                    return;
                }
                self.try_promote_to_active(correlation, rx).await;
            }
            (WorkerState::AwaitingReview, TrackerIssueState::Terminal) => {
                self.try_terminal_success(correlation).await;
            }
            (WorkerState::TerminalSuccess, TrackerIssueState::Terminal) => {
                self.try_cleaning(correlation).await;
            }
            (state, tracker) => {
                debug!(
                    target: "orchestrator",
                    issue = %self.key.as_str(),
                    actor_state = ?state,
                    tracker_state = ?tracker,
                    "tracker event ignored: no transition for current state",
                );
            }
        }
    }

    /// Run the `Queued -> Active` vetoable transition, then on `Allow` create
    /// the session workdir (NoOp shim post-7.1b), launch the engine, and
    /// drive the retry-budget Backoff loop until either (a) the engine
    /// reports `CleanExit` (advance to `AwaitingReview`), (b) the engine
    /// reports `Stalled` or `TurnBudgetExhausted` (route directly to
    /// `TerminalFailure` — these are agent-authored failures that repeat
    /// under the same prompt and budget, per SPEC.md §9.5), or (c) the
    /// configured `max_attempts` retry budget is exhausted by repeated
    /// `NonCleanExit` outcomes.
    ///
    /// The session workdir is retained across the Backoff loop — no
    /// delete/recreate between attempts. The same prelude /
    /// `additional_context` is re-emitted on each launch (failure-history
    /// accumulation is a downstream-spec concern, out of scope for the MVP).
    async fn try_promote_to_active(
        &self,
        correlation: CorrelationId,
        rx: &mut mpsc::Receiver<ActorCommand>,
    ) {
        let allowed = self
            .commit_transition(
                WorkerState::Queued,
                WorkerState::Active,
                TransitionTrigger::TrackerEvent,
                correlation,
            )
            .await;
        if !allowed {
            return;
        }

        // TODO(7.1d): replace this NoOp shim with
        // `SessionManager::create_session(&self.key)` once 7.1d lands. The
        // session tempdir lives at `~/Library/Caches/roki/sessions/<issue>`
        // on macOS / `~/.cache/roki/sessions/<issue>` on Linux per design
        // decision #5. Until then we hand the engine a placeholder path so
        // the orchestrator core's lifecycle still drives `Active` → engine
        // launch → terminal-outcome dispatch identically to the production
        // path. The engine launcher in the integration tests does not read
        // from the workspace_dir so the placeholder path is observationally
        // equivalent for unit-test purposes; the e2e tests that DO read
        // from it (`e2e_failure_retry`, `e2e_happy_path`) are temporarily
        // gated under `#[ignore]` until 7.1d wires the real session
        // tempdir.
        let workspace_dir = noop_session_workdir(&self.key);

        // Drive the retry-budget Backoff loop. The actor enters the loop in
        // `Active` (committed above) and re-enters `Active` after each
        // `Backoff -> Active` arc until one of the documented terminal arms
        // fires. Each iteration logs one `transition` per arc with the
        // attempt counter and the outcome so observability pipelines can
        // reconstruct the retry trace.
        loop {
            let attempt = self.read_consecutive_failures().saturating_add(1);
            info!(
                target: "orchestrator",
                issue = %self.key.as_str(),
                attempt,
                max_attempts = self.engine_policy.max_attempts,
                "launching worker (retry-budget loop)",
            );
            let outcome = self.launch_once(correlation, &workspace_dir).await;

            match outcome {
                Some(WorkerOutcome::CleanExit) => {
                    // Advance to `AwaitingReview` so the tracker can later
                    // promote to `TerminalSuccess`. Counter is not reset —
                    // the state machine forbids re-entering `Active` after
                    // `AwaitingReview`, so reset is unreachable.
                    self.commit_transition(
                        WorkerState::Active,
                        WorkerState::AwaitingReview,
                        TransitionTrigger::EngineEvent,
                        correlation,
                    )
                    .await;
                    return;
                }
                Some(WorkerOutcome::NonCleanExit { .. }) => {
                    let next_failures = self.increment_consecutive_failures();
                    let max_attempts = self.engine_policy.max_attempts;
                    if max_attempts <= next_failures {
                        // Budget exhausted: route Active -> TerminalFailure
                        // with the documented final-attempt fields.
                        info!(
                            target: "orchestrator",
                            issue = %self.key.as_str(),
                            final_attempt = next_failures,
                            max_attempts,
                            last_outcome_reason = "non_clean_exit",
                            "retry budget exhausted; escalating to TerminalFailure",
                        );
                        self.commit_transition(
                            WorkerState::Active,
                            WorkerState::TerminalFailure,
                            TransitionTrigger::EngineEvent,
                            correlation,
                        )
                        .await;
                        return;
                    }

                    // Budget remains: Active -> Backoff, sleep, Backoff -> Active.
                    let delay = self
                        .engine_policy
                        .next_launch_delay(WorkerOutcome::NonCleanExit { code: 0 }, next_failures);
                    info!(
                        target: "orchestrator",
                        issue = %self.key.as_str(),
                        attempt = next_failures,
                        delay_ms = delay.as_millis() as u64,
                        outcome_reason = "non_clean_exit",
                        "scheduling Backoff window before retry",
                    );
                    let advanced = self
                        .commit_transition(
                            WorkerState::Active,
                            WorkerState::Backoff,
                            TransitionTrigger::EngineEvent,
                            correlation,
                        )
                        .await;
                    if !advanced {
                        return;
                    }

                    // Sleep the Backoff window, but abort cleanly on shutdown
                    // or on an explicit Shutdown command from the orchestrator
                    // so the daemon honours its bounded shutdown contract.
                    if !self.sleep_backoff(delay, rx).await {
                        return;
                    }

                    let advanced = self
                        .commit_transition(
                            WorkerState::Backoff,
                            WorkerState::Active,
                            TransitionTrigger::EngineEvent,
                            correlation,
                        )
                        .await;
                    if !advanced {
                        return;
                    }
                    // Loop and try the next attempt.
                }
                Some(WorkerOutcome::TurnBudgetExhausted) | Some(WorkerOutcome::Stalled { .. }) => {
                    // Agent-authored failures: re-running with the same
                    // prompt and budget repeats the same outcome. Route
                    // directly to TerminalFailure with no Backoff cycle,
                    // matching SPEC.md §9.5.
                    let outcome_reason = match outcome {
                        Some(WorkerOutcome::TurnBudgetExhausted) => "turn_budget_exhausted",
                        Some(WorkerOutcome::Stalled { .. }) => "stalled",
                        _ => unreachable!(),
                    };
                    info!(
                        target: "orchestrator",
                        issue = %self.key.as_str(),
                        attempt,
                        last_outcome_reason = outcome_reason,
                        "agent-authored failure; routing directly to TerminalFailure",
                    );
                    self.commit_transition(
                        WorkerState::Active,
                        WorkerState::TerminalFailure,
                        TransitionTrigger::EngineEvent,
                        correlation,
                    )
                    .await;
                    return;
                }
                None => {
                    warn!(
                        target: "orchestrator",
                        issue = %self.key.as_str(),
                        "engine launch produced no terminal event; staying Active",
                    );
                    return;
                }
            }
        }
    }

    /// Run a single supervised launch cycle and return the terminal outcome.
    ///
    /// Owns the events channel so the orchestrator observes lifecycle events
    /// and the terminal `Exited` event without letting the per-actor task
    /// block on the engine's internal queue. Returns `None` if the supervised
    /// channel closed before producing an `Exited` event (treated as a
    /// programmer error in production paths).
    async fn launch_once(
        &self,
        correlation: CorrelationId,
        workspace_dir: &std::path::Path,
    ) -> Option<WorkerOutcome> {
        let (events_tx, mut events_rx) = mpsc::channel::<SupervisedEvent>(64);
        let engine = Arc::clone(&self.engine);
        let ctx = build_worker_context(
            self.key.clone(),
            correlation,
            workspace_dir.to_path_buf(),
            self.engine_policy,
        );
        let launch_handle = tokio::spawn(async move { engine.launch(ctx, events_tx).await });

        let mut terminal: Option<WorkerOutcome> = None;
        while let Some(event) = events_rx.recv().await {
            match event {
                SupervisedEvent::Lifecycle(_) => {
                    // Per-event observability is owned by the supervisor;
                    // the orchestrator does not act on individual lifecycle
                    // events for the MVP.
                }
                SupervisedEvent::Exited(outcome) => {
                    terminal = Some(outcome);
                    break;
                }
            }
        }
        let _ = launch_handle.await;
        terminal
    }

    /// Sleep the configured Backoff window. Returns `true` if the sleep
    /// completed normally (so the caller may proceed to relaunch); returns
    /// `false` if the sleep was preempted by a shutdown signal or by an
    /// explicit `ActorCommand::Shutdown` arriving on the actor inbox, in
    /// which case the caller must unwind and let the actor's outer loop
    /// observe shutdown / terminal-end on the next iteration.
    async fn sleep_backoff(&self, delay: Duration, rx: &mut mpsc::Receiver<ActorCommand>) -> bool {
        tokio::select! {
            biased;
            () = self.shutdown.wait() => {
                debug!(
                    target: "orchestrator",
                    issue = %self.key.as_str(),
                    "Backoff sleep aborted by shutdown signal",
                );
                false
            }
            cmd = rx.recv() => {
                if matches!(cmd, Some(ActorCommand::Shutdown) | None) {
                    debug!(
                        target: "orchestrator",
                        issue = %self.key.as_str(),
                        "Backoff sleep aborted by inbox shutdown",
                    );
                    false
                } else {
                    // Tracker events delivered during a Backoff window are
                    // intentionally dropped: the actor is committed to the
                    // current launch attempt and the next tracker event will
                    // be re-evaluated against the freshly resumed Active
                    // state once the loop continues. Keep sleeping for the
                    // remaining window — we approximate this by sleeping the
                    // full delay; missing a partial window here is acceptable
                    // because Backoff is a coarse-grained mechanism.
                    debug!(
                        target: "orchestrator",
                        issue = %self.key.as_str(),
                        "tracker event during Backoff window; sleeping out the remainder",
                    );
                    tokio::select! {
                        biased;
                        () = self.shutdown.wait() => false,
                        () = tokio::time::sleep(delay) => true,
                    }
                }
            }
            () = tokio::time::sleep(delay) => true,
        }
    }

    /// Read the actor's `consecutive_failures` counter from the state map.
    fn read_consecutive_failures(&self) -> u32 {
        let guard = self
            .state
            .read()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        guard
            .get(&self.key)
            .map(|record| record.consecutive_failures)
            .unwrap_or(0)
    }

    /// Increment `consecutive_failures` by one and return the new value.
    fn increment_consecutive_failures(&self) -> u32 {
        let mut guard = self
            .state
            .write()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        let entry = guard.entry(self.key.clone()).or_insert(ActorRecord {
            state: WorkerState::Active,
            last_event_at: None,
            last_correlation_id: None,
            inbox: None,
            consecutive_failures: 0,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.consecutive_failures
    }

    /// Run the vetoable `AwaitingReview -> TerminalSuccess` transition. On
    /// `Allow`, immediately attempt the next vetoable transition `TerminalSuccess
    /// -> Cleaning` (which itself runs the pre-cleanup hook chain).
    async fn try_terminal_success(&self, correlation: CorrelationId) {
        let allowed = self
            .commit_transition(
                WorkerState::AwaitingReview,
                WorkerState::TerminalSuccess,
                TransitionTrigger::TrackerEvent,
                correlation,
            )
            .await;
        if !allowed {
            return;
        }
        self.try_cleaning(correlation).await;
    }

    /// Run the vetoable `TerminalSuccess -> Cleaning` transition. The
    /// pre-cleanup hook chain is evaluated alongside the subscriber chain;
    /// either side returning `Deny` blocks the transition.
    async fn try_cleaning(&self, correlation: CorrelationId) {
        // Evaluate pre-cleanup hooks first. Per design.md, a Deny here keeps
        // the actor in TerminalSuccess so deferred-cleanup work can finish.
        let hook_ctx = PreCleanupContext::new(self.key.clone(), correlation);
        let hook_decision = self.hook_registry.evaluate_pre_cleanup(&hook_ctx).await;
        if let VetoDecision::Deny { reason } = hook_decision {
            info!(
                target: "orchestrator",
                issue = %self.key.as_str(),
                reason = %reason,
                "pre-cleanup hook denied TerminalSuccess -> Cleaning; staying in TerminalSuccess",
            );
            return;
        }

        let allowed = self
            .commit_transition(
                WorkerState::TerminalSuccess,
                WorkerState::Cleaning,
                TransitionTrigger::TrackerEvent,
                correlation,
            )
            .await;
        if !allowed {
            return;
        }

        // TODO(7.1d): replace this NoOp shim with a walk over
        // `WorktreeRegistry` for the worker that calls `wt.remove` per
        // registered worktree, then `SessionManager::remove_session(&self.key)`.
        // Per design decision #11 cleanup-on-Cleaning is the daemon's
        // responsibility (the agent does not call a `roki_close_worktree`
        // tool); the iteration order is registration order so per-arc logs
        // remain stable. A failure during the iteration must repopulate the
        // `poisoned` set so subsequent tracker events for the issue are
        // refused (mirrors the pre-7.1b workspace.remove failure path).
        debug!(
            target: "orchestrator",
            issue = %self.key.as_str(),
            "Cleaning entered; workspace teardown is a NoOp shim until 7.1d wires WorktreeRegistry",
        );
    }

    /// Read the actor's current state from the orchestrator state map.
    fn read_current_state(&self) -> WorkerState {
        let guard = self
            .state
            .read()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        guard
            .get(&self.key)
            .map(|record| record.state)
            .unwrap_or(WorkerState::Discovered)
    }

    /// Build a [`TransitionEvent`] for `(previous, next)` and route it through
    /// the event bus. Returns `true` iff the transition was allowed and
    /// committed; returns `false` if the transition was vetoed (subscriber
    /// chain) or if `(previous, next)` is not legal.
    async fn commit_transition(
        &self,
        previous: WorkerState,
        next: WorkerState,
        trigger: TransitionTrigger,
        correlation: CorrelationId,
    ) -> bool {
        let event =
            match TransitionEvent::new(self.key.clone(), previous, next, trigger, correlation) {
                Some(event) => event,
                None => {
                    warn!(
                        target: "orchestrator",
                        issue = %self.key.as_str(),
                        ?previous,
                        ?next,
                        "illegal transition rejected before publish",
                    );
                    return false;
                }
            };

        let decision = self.event_bus.publish(event).await;
        match decision {
            VetoDecision::Allow => {
                self.write_state(next, correlation);
                true
            }
            VetoDecision::Deny { reason } => {
                info!(
                    target: "orchestrator",
                    issue = %self.key.as_str(),
                    ?previous,
                    ?next,
                    reason = %reason,
                    "vetoable transition denied; staying in previous state",
                );
                false
            }
        }
    }

    /// Update the state map with the new state and refresh the
    /// last-event-at / last-correlation-id projection fields.
    fn write_state(&self, next: WorkerState, correlation: CorrelationId) {
        let mut guard = self
            .state
            .write()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        let entry = guard.entry(self.key.clone()).or_insert(ActorRecord {
            state: next,
            last_event_at: None,
            last_correlation_id: None,
            inbox: None,
            consecutive_failures: 0,
        });
        entry.state = next;
        entry.last_event_at = Some(SystemTime::now());
        entry.last_correlation_id = Some(correlation);
    }

    /// Drop the actor's inbox sender from the state map so the orchestrator
    /// runtime stops trying to forward tracker events to a dead actor.
    fn clear_inbox(&self) {
        let mut guard = self
            .state
            .write()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        if let Some(record) = guard.get_mut(&self.key) {
            record.inbox = None;
        }
    }
}

/// Build a placeholder session-workdir path keyed by issue id.
///
/// TODO(7.1d): replace with `SessionManager::create_session(issue)` returning
/// `~/Library/Caches/roki/sessions/<issue>` on macOS / `~/.cache/roki/sessions/<issue>`
/// on Linux. The current placeholder is intentionally not materialised on
/// disk — the engine launchers used in the orchestrator's unit + integration
/// tests do not require a real directory, and the e2e tests that DO require
/// one (`e2e_happy_path`, `e2e_failure_retry`) are gated under `#[ignore]`
/// pending 7.1d.
fn noop_session_workdir(issue: &IssueId) -> PathBuf {
    // Use the system temp dir as a host so `Path::parent`/`is_absolute`
    // semantics match what 7.1d's real `SessionManager` will return. The
    // path is NOT created and may not exist; consumers that probe disk for
    // it will fail. That is intentional during the 7.1b → 7.1d window.
    std::env::temp_dir()
        .join("roki-sessions-noop")
        .join(issue.as_str())
}

/// Build the per-launch [`WorkerContext`] supplied to the engine adapter.
///
/// Tool registry, permission, and policy fields are populated with safe
/// defaults for the MVP: task 3.4 will inject the live tool registry, and
/// task 3.5 will thread workflow-derived policy and permission through here.
/// The current defaults match what
/// [`crate::engine::ClaudeEngineAdapter`] would already accept and let the
/// orchestrator core land without crossing into 3.4 / 3.5 territory.
///
/// Per task 7.1b the [`WorkerContext`] still carries a `repo: RepoId` field
/// for backwards compatibility with the engine surface (`engine/mod.rs` is
/// outside this task's boundary). We populate it with a placeholder value
/// because the state-machine no longer keys by repo; 7.1d removes this
/// field from `WorkerContext` when the agent-driven repo selection lands.
fn build_worker_context(
    issue: IssueId,
    correlation: CorrelationId,
    workspace_dir: PathBuf,
    policy: EnginePolicy,
) -> WorkerContext {
    WorkerContext {
        // TODO(7.1d): drop `repo` from `WorkerContext`; it is no longer in
        // the state-machine key. The engine surface is outside the 7.1b
        // boundary so we populate it with a placeholder for now.
        repo: RepoId::new(""),
        issue,
        correlation_id: correlation,
        workspace_dir,
        prompt: String::new(),
        tool_catalog: Vec::new(),
        permission: ResolvedPermission {
            mode: PermissionMode::Allowlist {
                settings_path: PathBuf::new(),
            },
            sandbox: SandboxMode::WorkspaceWrite,
            elicitations: ElicitationsMode::Reject,
            mode_source: PermissionSource::Operator,
        },
        policy,
        additional_context: None,
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the worker actor's state-machine driver.
    //!
    //! Subprocess-style integration scenarios (full happy path with a
    //! recording subscriber, mid-run snapshot) live in the integration test
    //! at `tests/orchestrator_core.rs`. The unit tests here focus on the
    //! actor's local behaviour — illegal transitions, veto handling — so
    //! a regression in the driver surfaces without booting the daemon.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Minimal engine stub: returns the configured outcome and emits a
    /// single terminal Exited event per launch.
    struct StubEngine {
        outcome: WorkerOutcome,
        launches: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl EngineLauncher for StubEngine {
        async fn launch(
            &self,
            _ctx: WorkerContext,
            events: mpsc::Sender<SupervisedEvent>,
        ) -> Result<WorkerOutcome, LaunchError> {
            self.launches.fetch_add(1, AtomicOrdering::SeqCst);
            let _ = events.send(SupervisedEvent::Exited(self.outcome)).await;
            Ok(self.outcome)
        }
    }

    /// Stub workspace that succeeds on every call. The orchestrator core no
    /// longer invokes `ensure`/`remove` (post-7.1b NoOp shims) but the
    /// `with_recovery` constructor still requires a `Workspace` until 7.1d
    /// drops the trait dependency. The stub returns empty results so unit
    /// tests focused on the state-machine driver do not need to materialise
    /// any directories.
    fn workspace_for_test() -> Arc<dyn Workspace> {
        use async_trait::async_trait;
        use std::path::PathBuf;

        struct StubWorkspace;

        #[async_trait]
        impl Workspace for StubWorkspace {
            async fn ensure(
                &self,
                _repo: &RepoId,
                _issue: &IssueId,
            ) -> Result<PathBuf, crate::workspace::WorkspaceError> {
                Ok(std::env::temp_dir().join("roki-stub-workspace"))
            }

            async fn remove(
                &self,
                _repo: &RepoId,
                _issue: &IssueId,
            ) -> Result<(), crate::workspace::WorkspaceError> {
                Ok(())
            }

            async fn list_existing(
                &self,
            ) -> Result<Vec<(RepoId, IssueId, PathBuf)>, crate::workspace::WorkspaceError>
            {
                Ok(Vec::new())
            }
        }

        Arc::new(StubWorkspace)
    }

    fn fresh_orchestrator(
        engine: Arc<dyn EngineLauncher>,
    ) -> (Orchestrator, mpsc::Sender<NormalizedIssue>, ShutdownSignal) {
        let workspace = workspace_for_test();
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let hook_registry = Arc::new(HookRegistry::new());
        let shutdown = ShutdownSignal::new();
        let (tx, rx) = mpsc::channel::<NormalizedIssue>(8);
        let orch = Orchestrator::new(
            workspace,
            engine,
            event_bus,
            hook_registry,
            shutdown.clone(),
            rx,
        );
        (orch, tx, shutdown)
    }

    fn issue_event(state: TrackerIssueState) -> NormalizedIssue {
        NormalizedIssue {
            repo: RepoId::new("repo-a"),
            issue: IssueId::new("ENG-1"),
            title: String::new(),
            description: String::new(),
            state,
            labels: Vec::new(),
            team_or_scope: "ENG".to_string(),
        }
    }

    #[tokio::test]
    async fn shutdown_returns_run_loop() {
        // Smoke check that `run` exits when shutdown fires before any tracker
        // event arrives. Without this guarantee the orchestrator would hang
        // forever in the bounded-shutdown loop.
        let engine = Arc::new(StubEngine {
            outcome: WorkerOutcome::CleanExit,
            launches: Arc::new(AtomicUsize::new(0)),
        });
        let (orch, _tx, shutdown) = fresh_orchestrator(engine);
        let handle = tokio::spawn(async move { orch.run().await });
        shutdown.trigger();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("run loop must exit on shutdown")
            .expect("run task must not panic");
    }

    #[tokio::test]
    async fn dispatch_creates_state_entry_for_new_key() {
        // First tracker event for an issue should insert a record into the
        // state map so OrchestratorRead can project it before the actor has
        // even processed the event.
        let engine = Arc::new(StubEngine {
            outcome: WorkerOutcome::CleanExit,
            launches: Arc::new(AtomicUsize::new(0)),
        });
        let (orch, tx, shutdown) = fresh_orchestrator(engine);
        let read_handle = orch.read_handle();
        let handle = tokio::spawn(async move { orch.run().await });

        tx.send(issue_event(TrackerIssueState::Active))
            .await
            .expect("send active");

        // Wait until the state map carries a record for the issue. The actor
        // may have advanced past Discovered, but the issue must exist.
        let saw_record = {
            let mut tries = 0;
            loop {
                let snap = read_handle.snapshot();
                if !snap.issues.is_empty() {
                    break true;
                }
                if 200 < tries {
                    break false;
                }
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        assert!(saw_record, "state map must record the new issue");

        shutdown.trigger();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }
}
