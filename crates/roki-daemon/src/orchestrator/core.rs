//! Orchestrator runtime: per-`(repo, issue)` worker actor and the supervising
//! event loop.
//!
//! Task 3.2 of the roki-mvp spec. This module owns the central
//! [`Orchestrator`] struct that:
//!
//! * holds the canonical in-memory state map (`Arc<RwLock<HashMap<(RepoId,
//!   IssueId), IssueState>>>`);
//! * spawns one tokio task per `(repo, issue)` — the per-issue worker actor;
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
//! ## What this module does NOT do
//!
//! Restart recovery (task 3.3), tool-registry-aware launches (3.4),
//! workspace-lifecycle wiring beyond `ensure`/`remove` around `Active`/
//! `Cleaning` (3.5), and the tracker→orchestrator bridge (3.6) are deferred
//! to their owner tasks. The [`Orchestrator`] takes a generic
//! [`mpsc::Receiver<NormalizedIssue>`] inbox so 3.6 can wire polling and
//! webhook adapters into it without modifying core.
//!
//! ## Boundary
//!
//! The orchestrator depends on a small [`EngineLauncher`] trait rather than a
//! concrete adapter so:
//!
//! * the integration test in `tests/orchestrator_core.rs` can stub engine
//!   launches without spawning real subprocesses;
//! * future work (3.4) that wires the tool registry through `WorkerContext`
//!   keeps a clean seam.
//!
//! [`ClaudeEngineAdapter::launch`] already matches this trait signature
//! (`async fn launch(&self, WorkerContext, mpsc::Sender<SupervisedEvent>) ->
//! Result<WorkerOutcome, LaunchError>`) so a wrapper impl can be added by 3.4
//! without breaking core.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::engine::policy::{EnginePolicy, WorkerOutcome};
use crate::engine::{SupervisedEvent, WorkerContext};
use crate::orchestrator::events::EventBus;
use crate::orchestrator::hooks::{HookRegistry, PreCleanupContext};
use crate::orchestrator::read::{IssueState, OrchestratorRead, SnapshotResponse};
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
/// `(repo, issue)` worker. Used by the `OrchestratorRead` projection.
#[derive(Debug, Clone)]
struct ActorRecord {
    state: WorkerState,
    last_event_at: Option<SystemTime>,
    last_correlation_id: Option<CorrelationId>,
    /// Sender into the per-actor inbox, used by the orchestrator runtime to
    /// forward tracker events. `None` once the actor has terminated.
    inbox: Option<mpsc::Sender<ActorCommand>>,
}

/// Commands routed into a per-`(repo, issue)` actor's inbox.
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
    state: Arc<RwLock<HashMap<(RepoId, IssueId), ActorRecord>>>,
}

impl OrchestratorRead for OrchestratorReadHandle {
    fn snapshot(&self) -> SnapshotResponse {
        let guard = self
            .state
            .read()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        let issues: Vec<IssueState> = guard
            .iter()
            .map(|((repo, issue), record)| IssueState {
                repo: repo.clone(),
                issue: issue.clone(),
                state: record.state,
                last_event_at: record.last_event_at,
                last_correlation_id: record.last_correlation_id,
            })
            .collect();
        SnapshotResponse::new(issues)
    }

    fn issue(&self, repo: &RepoId, issue: &IssueId) -> Option<IssueState> {
        let guard = self
            .state
            .read()
            .expect("orchestrator state RwLock poisoned; this is unrecoverable");
        guard
            .get(&(repo.clone(), issue.clone()))
            .map(|record| IssueState {
                repo: repo.clone(),
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
    workspace: Arc<dyn Workspace>,
    engine: Arc<dyn EngineLauncher>,
    event_bus: Arc<EventBus>,
    hook_registry: Arc<HookRegistry>,
    shutdown: ShutdownSignal,
    tracker_inbox: mpsc::Receiver<NormalizedIssue>,
    state: Arc<RwLock<HashMap<(RepoId, IssueId), ActorRecord>>>,
}

impl Orchestrator {
    /// Construct a new orchestrator. The caller is expected to inject the
    /// canonical singletons (workspace, engine, event bus, hook registry,
    /// shutdown signal) and a `tracker_inbox` receiver fed by the
    /// tracker→orchestrator bridge (task 3.6).
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
        }
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
    /// this is the first time we see the `(repo, issue)` key.
    async fn dispatch_tracker_event(&mut self, issue: NormalizedIssue) {
        let key = (issue.repo.clone(), issue.issue.clone());
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
                repo = %key.0.as_str(),
                issue = %key.1.as_str(),
                "actor inbox closed before tracker event could be delivered",
            );
        }
    }

    /// Spawn the per-`(repo, issue)` actor task.
    fn spawn_actor(&self, key: (RepoId, IssueId), rx: mpsc::Receiver<ActorCommand>) {
        let actor = WorkerActor {
            key,
            state: Arc::clone(&self.state),
            workspace: Arc::clone(&self.workspace),
            engine: Arc::clone(&self.engine),
            event_bus: Arc::clone(&self.event_bus),
            hook_registry: Arc::clone(&self.hook_registry),
        };
        tokio::spawn(async move { actor.run(rx).await });
    }
}

/// Per-`(repo, issue)` worker actor.
///
/// Owns one key, drives the state machine `Discovered -> Queued -> Active ->
/// AwaitingReview -> TerminalSuccess -> Cleaning -> [*]` through tracker and
/// engine events. Every committed transition is published through
/// [`EventBus::publish`]; the three vetoable transitions are gated through
/// the bus's vetoable path (and the `TerminalSuccess -> Cleaning` transition
/// also through the [`HookRegistry`]).
struct WorkerActor {
    key: (RepoId, IssueId),
    state: Arc<RwLock<HashMap<(RepoId, IssueId), ActorRecord>>>,
    workspace: Arc<dyn Workspace>,
    engine: Arc<dyn EngineLauncher>,
    event_bus: Arc<EventBus>,
    hook_registry: Arc<HookRegistry>,
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
                    repo = %self.key.0.as_str(),
                    issue = %self.key.1.as_str(),
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
                        repo = %self.key.0.as_str(),
                        issue = %self.key.1.as_str(),
                        "actor inbox closed; exiting",
                    );
                    return;
                }
            };

            match command {
                ActorCommand::Shutdown => {
                    info!(
                        target: "orchestrator",
                        repo = %self.key.0.as_str(),
                        issue = %self.key.1.as_str(),
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
        _rx: &mut mpsc::Receiver<ActorCommand>,
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
                self.try_promote_to_active(correlation).await;
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
                    repo = %self.key.0.as_str(),
                    issue = %self.key.1.as_str(),
                    actor_state = ?state,
                    tracker_state = ?tracker,
                    "tracker event ignored: no transition for current state",
                );
            }
        }
    }

    /// Run the `Queued -> Active` vetoable transition, then on `Allow` create
    /// the workspace, launch the engine, and wait for the terminal supervised
    /// event so the actor can promote to `AwaitingReview`.
    async fn try_promote_to_active(&self, correlation: CorrelationId) {
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

        // Workspace lifecycle: ensure the directory exists before the worker
        // launches. Failure to materialise the workspace is a hard fault for
        // the actor — log and stay in Active without launching a worker.
        let workspace_dir = match self.workspace.ensure(&self.key.0, &self.key.1).await {
            Ok(path) => path,
            Err(err) => {
                warn!(
                    target: "orchestrator",
                    repo = %self.key.0.as_str(),
                    issue = %self.key.1.as_str(),
                    error = %err,
                    "workspace ensure failed; skipping engine launch",
                );
                return;
            }
        };

        // Launch the engine. The orchestrator owns the events channel so it
        // can observe lifecycle events and the terminal Exited event without
        // letting the per-actor task block on the engine's internal queue.
        let (events_tx, mut events_rx) = mpsc::channel::<SupervisedEvent>(64);
        let engine = Arc::clone(&self.engine);
        let ctx = build_worker_context(
            self.key.0.clone(),
            self.key.1.clone(),
            correlation,
            workspace_dir.clone(),
        );
        let launch_handle = tokio::spawn(async move { engine.launch(ctx, events_tx).await });

        // Drive the supervised event loop. Per the engine contract, exactly
        // one terminal Exited event is emitted per successful launch.
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
        // Ensure the launch task resolves so we don't leak its handle.
        let _ = launch_handle.await;

        match terminal {
            Some(WorkerOutcome::CleanExit) => {
                // Move into AwaitingReview so the tracker can later promote
                // to TerminalSuccess once the issue is resolved.
                self.commit_transition(
                    WorkerState::Active,
                    WorkerState::AwaitingReview,
                    TransitionTrigger::EngineEvent,
                    correlation,
                )
                .await;
            }
            Some(WorkerOutcome::NonCleanExit { .. })
            | Some(WorkerOutcome::TurnBudgetExhausted)
            | Some(WorkerOutcome::Stalled { .. }) => {
                self.commit_transition(
                    WorkerState::Active,
                    WorkerState::TerminalFailure,
                    TransitionTrigger::EngineEvent,
                    correlation,
                )
                .await;
            }
            None => {
                warn!(
                    target: "orchestrator",
                    repo = %self.key.0.as_str(),
                    issue = %self.key.1.as_str(),
                    "engine launch produced no terminal event; staying Active",
                );
            }
        }
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
        let hook_ctx = PreCleanupContext::new(self.key.0.clone(), self.key.1.clone(), correlation);
        let hook_decision = self.hook_registry.evaluate_pre_cleanup(&hook_ctx).await;
        if let VetoDecision::Deny { reason } = hook_decision {
            info!(
                target: "orchestrator",
                repo = %self.key.0.as_str(),
                issue = %self.key.1.as_str(),
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

        // Workspace removal happens once the actor enters Cleaning. Failure
        // is logged but does not block actor termination — the directory
        // will be picked up by the next recovery scan if it lingers.
        if let Err(err) = self.workspace.remove(&self.key.0, &self.key.1).await {
            warn!(
                target: "orchestrator",
                repo = %self.key.0.as_str(),
                issue = %self.key.1.as_str(),
                error = %err,
                "workspace remove failed during Cleaning",
            );
        }
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
        let event = match TransitionEvent::new(
            self.key.0.clone(),
            self.key.1.clone(),
            previous,
            next,
            trigger,
            correlation,
        ) {
            Some(event) => event,
            None => {
                warn!(
                    target: "orchestrator",
                    repo = %self.key.0.as_str(),
                    issue = %self.key.1.as_str(),
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
                    repo = %self.key.0.as_str(),
                    issue = %self.key.1.as_str(),
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

/// Build the per-launch [`WorkerContext`] supplied to the engine adapter.
///
/// Tool registry, permission, and policy fields are populated with safe
/// defaults for the MVP: task 3.4 will inject the live tool registry, and
/// task 3.5 will thread workflow-derived policy and permission through here.
/// The current defaults match what
/// [`crate::engine::ClaudeEngineAdapter`] would already accept and let the
/// orchestrator core land without crossing into 3.4 / 3.5 territory.
fn build_worker_context(
    repo: RepoId,
    issue: IssueId,
    correlation: CorrelationId,
    workspace_dir: PathBuf,
) -> WorkerContext {
    WorkerContext {
        repo,
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
        policy: EnginePolicy::default(),
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
    use tempfile::tempdir;

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

    fn workspace_for_test() -> Arc<dyn Workspace> {
        let dir = tempdir().expect("tempdir for workspace root");
        // Leak the tempdir so the directory survives for the test's lifetime;
        // an orchestrator unit test that never persists across runs is
        // tolerant of the deliberate leak.
        let path = dir.keep();
        Arc::new(crate::workspace::WorkspaceManager::new(path).expect("workspace manager"))
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
        // First tracker event for a key should insert a record into the
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

        // Wait until the state map carries a record for the key. The actor
        // may have advanced past Discovered, but the key must exist.
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
        assert!(
            saw_record,
            "state map must record the new (repo, issue) key"
        );

        shutdown.trigger();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }
}
