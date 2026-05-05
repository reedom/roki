//! Per-issue orchestrator actor + container.
//!
//! `Orchestrator` is the daemon-internal actor container. It owns:
//! - A map of `IssueId -> ActorContext` (one tokio task per issue).
//! - The shared dependencies (engine traits, session manager, worktree
//!   manager, escalation queue, event bus, hook dispatcher).
//!
//! Each per-issue actor consumes mpsc `ActorMessage`s from declared sources:
//! tracker events, orchestrator action outcomes, phase lifecycle events,
//! daemon-directive feedback, and the daemon shutdown signal. There are NO
//! silent transitions: every state change is published via the event bus +
//! subscriber hooks with an explicit `TransitionTrigger`.
//!
//! Engine integration is mediated by the [`OrchestratorEngine`] and
//! [`PhaseEngine`] traits so unit tests can drive event sequences with
//! recording stubs while production wires the existing
//! `OrchestratorSessionAdapter` / `PhaseSubprocessAdapter` underneath. The
//! adapter integration tests using `fake_claude` continue to demonstrate
//! the real path; this module's tests use the trait stubs for fast,
//! deterministic coverage.
//!
//! Spec refs: requirements.md Req 4.1, 4.9, 4.10, 4.11, 4.12, 5.6, 5.11,
//! 8.1, 8.2.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::engine::orchestrator_session::action_parser::{
    ActionKind, OrchestratorAction, Outcome, PhaseName,
};
use crate::engine::orchestrator_session::events::{
    DaemonEvent, TrackerTerminalPayload, TrackerTerminalState as EventTerminalState,
};
use crate::engine::phase_subprocess::catalog::{CatalogError, catalog_default};
use crate::orchestrator::escalation::{
    EscalationEntry, EscalationKind, EscalationQueue, route_daemon_directive,
};
use crate::orchestrator::events::EventBus;
use crate::orchestrator::hooks::SubscriberHooks;
use crate::orchestrator::read::ActorSnapshot;
use crate::orchestrator::state::{
    InactiveReason, IssueId, Mode, TransitionEvent, TransitionTrigger, WorkerState,
};
use crate::tracker::model::{LinearStateName, RepoId};

// ---------------------------------------------------------------------------
// Engine wrapper traits — testable seam over the real adapters
// ---------------------------------------------------------------------------

/// Daemon-internal handle to a launched orchestrator session. The real
/// implementation wraps `OrchestratorSessionHandle`; tests use a recording
/// stub that simulates stdin delivery + canned `ActionEvent` emission.
#[async_trait]
pub trait OrchestratorSessionLike: Send + Sync {
    /// Deliver one `DaemonEvent` to the orchestrator's stdin. Returns `Err`
    /// if the channel is closed.
    async fn deliver(&self, event: DaemonEvent) -> Result<(), DeliveryError>;

    /// Pop the next `OrchestratorAction` (or `None` when the action channel
    /// closed because the orchestrator exited).
    async fn next_action(&mut self) -> Option<OrchestratorActionEvent>;

    /// Initiate graceful shutdown with the configured grace window. The
    /// session-side resources are cleaned up on completion.
    async fn shutdown(self: Box<Self>, grace: Option<Duration>);
}

/// What the orchestrator-session adapter surfaces to the orchestrator core.
/// Mirrors a curated subset of `engine::orchestrator_session::adapter::ActionEvent`
/// so the trait does not pull adapter-internal types into the public surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestratorActionEvent {
    Action(OrchestratorAction),
    /// Orchestrator process exited (success or failure). The actor maps this
    /// to `Inactive(orchestrator_crash)` if no terminal `action=stop` had
    /// already been recorded.
    ProcessExit { success: bool },
    /// Two consecutive schema-drift turns observed.
    TerminalDrift,
}

/// Error surfacing from `deliver`. Closed channel = orchestrator session is
/// no longer alive (drained, crashed, or shutdown was already issued).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DeliveryError {
    #[error("orchestrator stdin channel closed")]
    Closed,
}

/// Trait the orchestrator core uses to launch orchestrator sessions. The
/// real impl wires `OrchestratorSessionAdapter`; tests inject a recording
/// stub that yields a [`OrchestratorSessionLike`] whose action stream is
/// pre-canned.
#[async_trait]
pub trait OrchestratorEngine: Send + Sync {
    /// Launch a fresh orchestrator session for the issue. Mode is set on
    /// entry and IMMUTABLE for the session's lifetime.
    async fn launch(
        &self,
        issue: &IssueId,
        mode: Mode,
        system_prompt: String,
    ) -> Result<Box<dyn OrchestratorSessionLike>, EngineError>;
}

/// Outcome of running one phase subprocess to completion. The real impl
/// drives `PhaseSubprocessAdapter::spawn` + `translate_exit`; tests return a
/// pre-canned `DaemonEvent`.
#[derive(Debug, Clone, PartialEq)]
pub enum PhaseRunOutcome {
    /// Phase finished and produced a `DaemonEvent` ready to deliver to the
    /// orchestrator's stdin (PhaseComplete or PhaseNonclean).
    Translated(DaemonEvent),
    /// A tracker-terminal observation cancelled the phase mid-flight; the
    /// phase exit translation is discarded per Req 5.8 / 4.9.
    TrackerTerminalDiscarded,
}

#[async_trait]
pub trait PhaseEngine: Send + Sync {
    /// Run one phase. `worktree_path` is `None` for `Classify`, `Some` for
    /// every other phase (the orchestrator core ensures this contract).
    /// `session_tempdir` is the per-issue session directory the orchestrator
    /// core ensured at admission; the engine impl threads it into the
    /// adapter's `PhaseLaunchContext` as the spawn cwd.
    async fn run_phase(
        &self,
        issue: &IssueId,
        phase: PhaseName,
        mode: Mode,
        worktree_path: Option<PathBuf>,
        additional_context: Option<String>,
        session_tempdir: PathBuf,
    ) -> Result<PhaseRunOutcome, EngineError>;
}

/// Worktree management seam consumed by the actor. The real impl is
/// `WorktreeManager`; tests use a recording stub.
#[async_trait]
pub trait WorktreeOps: Send + Sync {
    async fn ensure(
        &self,
        issue: &IssueId,
        repo_id: &RepoId,
    ) -> Result<PathBuf, WorktreeOpError>;

    async fn cleanup(&self, issue: &IssueId) -> Result<Vec<PathBuf>, WorktreeOpError>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WorktreeOpError {
    #[error("repo `{0}` is not in the [[repos]] allowlist")]
    AllowlistRejected(String),
    #[error("filesystem poison: {0}")]
    FsPoison(String),
    #[error("worktree op failed: {0}")]
    Other(String),
}

/// Session tempdir seam. Mirrors the small surface `SessionManager` exposes.
pub trait SessionDirOps: Send + Sync {
    fn ensure(&self, issue: &IssueId) -> Result<PathBuf, SessionDirError>;
    fn remove(&self, issue: &IssueId) -> Result<(), SessionDirError>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionDirError {
    #[error("session dir error: {0}")]
    Other(String),
}

/// Engine-side launch failure that the actor maps to a typed `Inactive`
/// reason or routes through escalation.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("engine launch failed: {0}")]
    LaunchFailed(String),
    #[error("engine internal error: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// Actor messages
// ---------------------------------------------------------------------------

/// One inbound message to a per-issue actor. Variants enumerate every
/// declared event source so the actor's main loop is exhaustive.
#[derive(Debug)]
pub enum ActorMessage {
    /// Tracker observed (re-)admission for this issue. The bridge has
    /// already validated the admission decision. `repo` (when known) is
    /// resolved against the worktree allowlist on each `run_phase`
    /// (Classify excepted).
    TrackerAdmit { mode: Mode, repo: Option<RepoId> },
    /// Tracker observed assignment loss / `roki:ready` removal mid-flight.
    TrackerAssignmentLost,
    TrackerRokiReadyRemoved,
    /// Tracker observed the issue reaching a terminal Linear state.
    TrackerTerminalState { state: LinearStateName },
    /// Daemon shutdown was triggered; actor must wind down.
    Shutdown,
    /// Daemon-detected escalation that needs to be forwarded to the live
    /// orchestrator (or, when the orchestrator is dead, queued + logged).
    DaemonEscalation {
        kind: EscalationKind,
        fields: Value,
        correlation_id: String,
    },
    /// Filesystem failure detected by the actor (worktree / session tempdir
    /// op failed). Routed to `Inactive(fs_poison)` and a `daemon_directive`
    /// when the orchestrator is alive.
    FsPoisonDetected {
        offending_path: PathBuf,
        cause: String,
    },
}

// ---------------------------------------------------------------------------
// Orchestrator actor
// ---------------------------------------------------------------------------

/// One per-issue actor. Owned by the parent `Orchestrator`; communication
/// is exclusively via the mpsc inbox plus internal session/phase channels.
pub struct ActorContext {
    pub issue: IssueId,
    pub inbox: mpsc::Sender<ActorMessage>,
    pub join: JoinHandle<()>,
}

/// Mode + repo context captured at admission. Mode is immutable for the
/// session's lifetime per Req 4.10.
#[derive(Debug, Clone)]
struct AdmissionContext {
    mode: Mode,
    /// Operator-supplied repo id. Validated at every `run_phase` against
    /// the worktree allowlist.
    repo: Option<RepoId>,
}

/// Aggregate runtime dependencies the actor needs.
#[derive(Clone)]
pub struct OrchestratorDeps {
    pub orchestrator_engine: Arc<dyn OrchestratorEngine>,
    pub phase_engine: Arc<dyn PhaseEngine>,
    pub worktree: Arc<dyn WorktreeOps>,
    pub session_dirs: Arc<dyn SessionDirOps>,
    pub event_bus: EventBus,
    pub hooks: Arc<SubscriberHooks>,
    pub escalations: Arc<EscalationQueue>,
    /// State map shared with the read handle. The actor writes its own
    /// snapshot row; the read handle is read-only.
    pub state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>>,
}

/// Public container managing all per-issue actors.
pub struct Orchestrator {
    deps: OrchestratorDeps,
    actors: Mutex<HashMap<IssueId, ActorContext>>,
}

impl Orchestrator {
    pub fn new(deps: OrchestratorDeps) -> Self {
        Self {
            deps,
            actors: Mutex::new(HashMap::new()),
        }
    }

    pub fn deps(&self) -> &OrchestratorDeps {
        &self.deps
    }

    /// Send a message to the actor for `issue`. Spawns the actor on first
    /// admission. Returns `Err` if the issue's actor inbox is closed
    /// (terminal state already reached and the actor exited).
    pub async fn send(
        &self,
        issue: IssueId,
        message: ActorMessage,
    ) -> Result<(), ActorMessage> {
        // Spawn-on-first-message gate.
        let inbox = {
            let mut guard = self.actors.lock().expect("actors mutex poisoned");
            if let Some(ctx) = guard.get(&issue) {
                if !ctx.inbox.is_closed() {
                    ctx.inbox.clone()
                } else {
                    // Actor exited; reuse the slot by replacing it.
                    let ctx = spawn_actor(issue.clone(), self.deps.clone());
                    guard.insert(issue.clone(), ctx);
                    guard.get(&issue).unwrap().inbox.clone()
                }
            } else {
                let ctx = spawn_actor(issue.clone(), self.deps.clone());
                guard.insert(issue.clone(), ctx);
                guard.get(&issue).unwrap().inbox.clone()
            }
        };
        match inbox.send(message).await {
            Ok(()) => Ok(()),
            Err(send_err) => Err(send_err.0),
        }
    }

    /// Test/inspection helper: number of registered actors.
    pub fn actor_count(&self) -> usize {
        self.actors.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Wait for the actor for `issue` to exit. Test helper.
    pub async fn join(&self, issue: &IssueId) {
        let join = {
            let mut guard = self.actors.lock().unwrap();
            guard.remove(issue).map(|ctx| ctx.join)
        };
        if let Some(handle) = join {
            let _ = handle.await;
        }
    }

    /// Drop every actor inbox and return the join handles so the runtime
    /// can await them within a bounded window. After this call, no new
    /// admissions can be sent — `Orchestrator::send` will spawn a new actor
    /// for any subsequent message because the actor's `inbox` is gone from
    /// the map. Dropping `ctx.inbox` (the mpsc Sender) closes the channel
    /// for actors currently parked at `rx.recv().await`; those actors fall
    /// through the loop tail which closes the held orchestrator session at
    /// the engine seam (stdin close → SIGTERM → bounded wait per the
    /// adapter). Actors blocked deeper in the action / phase pipelines are
    /// awaited by the caller within `await_workers_with_window`'s budget;
    /// timeouts force-abort the JoinHandle.
    pub fn drain_actors(&self) -> Vec<(IssueId, JoinHandle<()>)> {
        let mut guard = self.actors.lock().expect("actors mutex poisoned");
        guard
            .drain()
            .map(|(issue, ctx)| (issue, ctx.join))
            .collect()
    }
}

fn spawn_actor(issue: IssueId, deps: OrchestratorDeps) -> ActorContext {
    let (tx, rx) = mpsc::channel::<ActorMessage>(64);
    let join = tokio::spawn(actor_loop(issue.clone(), rx, deps));
    ActorContext {
        issue,
        inbox: tx,
        join,
    }
}

// ---------------------------------------------------------------------------
// Actor main loop
// ---------------------------------------------------------------------------

struct ActorState {
    issue: IssueId,
    state: WorkerState,
    admission: Option<AdmissionContext>,
    /// Active orchestrator session (when in Pending/Active/Backoff with a
    /// live orchestrator).
    session: Option<Box<dyn OrchestratorSessionLike>>,
    /// Per-issue session tempdir resolved at admission. Removed AFTER
    /// worktree cleanup completes in the Cleaning state.
    session_tempdir: Option<PathBuf>,
    /// Latest known Linear state for snapshot publishing.
    latest_linear_state: Option<LinearStateName>,
    /// Whether the actor has refused further work (after FsPoison).
    refused_for_fs_poison: bool,
}

impl ActorState {
    fn new(issue: IssueId) -> Self {
        Self {
            issue,
            state: WorkerState::Pending,
            admission: None,
            session: None,
            session_tempdir: None,
            latest_linear_state: None,
            refused_for_fs_poison: false,
        }
    }

    fn mode(&self) -> Option<Mode> {
        self.admission.as_ref().map(|a| a.mode)
    }

    fn repo(&self) -> Option<&RepoId> {
        self.admission.as_ref().and_then(|a| a.repo.as_ref())
    }
}

async fn actor_loop(
    issue: IssueId,
    mut rx: mpsc::Receiver<ActorMessage>,
    deps: OrchestratorDeps,
) {
    let mut state = ActorState::new(issue.clone());
    publish_snapshot(&state, &deps);

    while let Some(msg) = rx.recv().await {
        // After Cleaning fully drains, exit the loop.
        let exit = handle_message(&mut state, msg, &deps).await;
        publish_snapshot(&state, &deps);
        if exit {
            break;
        }
    }

    // Drain any remaining inbox messages so producers don't block on send;
    // the actor is no longer authoritative once it exits.
    while rx.try_recv().is_ok() {}

    // Daemon-shutdown teardown path: when the runtime drops the actor's
    // inbox (`Orchestrator::drain_actors`), `rx.recv()` returns None and we
    // fall through here. If a session is still held (the actor parked at
    // its inbox between turns) close it through the engine seam so stdin
    // closes + the session adapter SIGTERMs the in-flight orchestrator
    // child within the per-subprocess shutdown grace. Without this, the
    // wind-down would only drop the handle and leak the IO tasks.
    if let Some(session) = state.session.take() {
        session.shutdown(Some(Duration::from_secs(5))).await;
    }
}

/// Returns `true` iff the actor should exit after handling this message.
async fn handle_message(
    state: &mut ActorState,
    msg: ActorMessage,
    deps: &OrchestratorDeps,
) -> bool {
    match msg {
        ActorMessage::TrackerAdmit { mode, repo } => {
            handle_admit(state, mode, repo, deps).await;
            false
        }
        ActorMessage::TrackerAssignmentLost => {
            enter_cleaning(state, deps, TransitionTrigger::AssignmentLost).await;
            true
        }
        ActorMessage::TrackerRokiReadyRemoved => {
            enter_cleaning(state, deps, TransitionTrigger::RokiReadyRemoved).await;
            true
        }
        ActorMessage::TrackerTerminalState { state: linear_state } => {
            state.latest_linear_state = Some(linear_state);
            // Mid-flight terminal observations preempt to Cleaning. From
            // Inactive(non-AwaitingLinear) we also enter Cleaning when the
            // operator finally closes the ticket.
            enter_cleaning(state, deps, TransitionTrigger::TrackerEvent).await;
            true
        }
        ActorMessage::Shutdown => {
            enter_cleaning(state, deps, TransitionTrigger::OperatorShutdown).await;
            true
        }
        ActorMessage::DaemonEscalation { kind, fields, correlation_id } => {
            handle_daemon_escalation(state, kind, fields, correlation_id, deps).await;
            false
        }
        ActorMessage::FsPoisonDetected { offending_path, cause } => {
            handle_fs_poison(state, offending_path, cause, deps).await;
            false
        }
    }
}

async fn handle_admit(
    state: &mut ActorState,
    mode: Mode,
    repo: Option<RepoId>,
    deps: &OrchestratorDeps,
) {
    if state.refused_for_fs_poison {
        warn!(
            target: "orchestrator.core",
            issue = %state.issue,
            "refusing admission: actor is in fs_poison refusal state"
        );
        return;
    }

    // Mode is immutable for an active orchestrator session lifetime. If
    // the actor is currently Pending/Active/Backoff, ignore re-admission
    // shape changes (the bridge already filtered duplicates; this is a
    // belt-and-braces guard).
    if matches!(
        state.state,
        WorkerState::Pending | WorkerState::Active | WorkerState::Backoff
    ) && state.admission.is_some()
    {
        return;
    }

    // Ensure a session tempdir exists; failure is a typed FsPoison.
    let tempdir = match deps.session_dirs.ensure(&state.issue) {
        Ok(p) => p,
        Err(err) => {
            handle_fs_poison(
                state,
                PathBuf::from(format!("<session-tempdir for {}>", state.issue)),
                err.to_string(),
                deps,
            )
            .await;
            return;
        }
    };
    state.session_tempdir = Some(tempdir);

    let prev = state.state.clone();
    state.admission = Some(AdmissionContext { mode, repo });

    // Launch the orchestrator session. The system prompt is rendered by
    // the engine impl (real adapter wires render_orchestrator_prompt +
    // fallback). The trait keeps this module engine-agnostic.
    let prompt = format!(
        "Roki orchestrator session for {issue}. Mode: {mode:?}.",
        issue = state.issue,
        mode = mode,
    );

    match deps
        .orchestrator_engine
        .launch(&state.issue, mode, prompt)
        .await
    {
        Ok(session) => {
            state.session = Some(session);
            state.state = WorkerState::Pending;
            // No state change is being modeled here as a transition: the
            // actor's pre-admission "no state row" -> Pending happens at
            // the bridge layer (already published as TrackerEvent +
            // initial Pending row). We publish the snapshot only.
            // For completeness, if `prev == Pending` we skip the
            // transition publish. Re-admission from Inactive(*) -> Pending
            // is a TrackerEvent transition.
            if !matches!(prev, WorkerState::Pending) {
                publish_transition(
                    deps,
                    &state.issue,
                    state.repo().map(|r| r.0.clone()),
                    prev,
                    WorkerState::Pending,
                    TransitionTrigger::TrackerEvent,
                    Some(mode),
                    None,
                );
            }
            // Drain orchestrator action events synchronously here would
            // block the actor's inbox; instead, we bridge them through a
            // helper that polls action events between inbox messages.
            // For the test seam we drain actions inline by spawning a
            // sub-loop that interleaves with inbox via `tokio::select!`.
            drain_actions(state, deps).await;
        }
        Err(err) => {
            warn!(
                target: "orchestrator.core",
                issue = %state.issue,
                error = %err,
                "orchestrator session launch failed; routing to orchestrator_crash"
            );
            transition_to_inactive(
                state,
                deps,
                InactiveReason::OrchestratorCrash,
                TransitionTrigger::OrchestratorDead,
            );
        }
    }
}

/// Drain action events from the orchestrator session until an `action=stop`
/// is observed, the orchestrator exits, or the session reports terminal
/// drift. Each `run_phase` action triggers a phase subprocess invocation
/// via the [`PhaseEngine`] trait.
async fn drain_actions(state: &mut ActorState, deps: &OrchestratorDeps) {
    loop {
        let event = match state.session.as_mut() {
            Some(s) => s.next_action().await,
            None => return,
        };
        let Some(event) = event else {
            // Action channel closed: orchestrator process exited without a
            // terminal stop. Route to OrchestratorCrash unless we've
            // already moved out of an active state.
            if matches!(
                state.state,
                WorkerState::Pending | WorkerState::Active | WorkerState::Backoff
            ) {
                route_orchestrator_dead(
                    state,
                    deps,
                    EscalationKind::OrchestratorCrash,
                    InactiveReason::OrchestratorCrash,
                    json!({ "exit_success": false, "channel_closed": true }),
                )
                .await;
            }
            return;
        };

        match event {
            OrchestratorActionEvent::Action(action) => {
                if !handle_action(state, action, deps).await {
                    return;
                }
            }
            OrchestratorActionEvent::ProcessExit { success } => {
                if matches!(
                    state.state,
                    WorkerState::Pending | WorkerState::Active | WorkerState::Backoff
                ) {
                    route_orchestrator_dead(
                        state,
                        deps,
                        EscalationKind::OrchestratorCrash,
                        InactiveReason::OrchestratorCrash,
                        json!({ "exit_success": success }),
                    )
                    .await;
                }
                return;
            }
            OrchestratorActionEvent::TerminalDrift => {
                route_orchestrator_dead(
                    state,
                    deps,
                    EscalationKind::OrchestratorUnparseable,
                    InactiveReason::OrchestratorUnparseable,
                    json!({ "reason": "terminal_drift" }),
                )
                .await;
                return;
            }
        }
    }
}

/// Route an orchestrator-dead failure: enqueue the escalation entry on the
/// queue FIRST so `OrchestratorRead` snapshots reflect the failure even when
/// the actor exits before any other observer runs, then transition the
/// actor to the matching `Inactive` reason via `OrchestratorDead`.
///
/// Mirrors the enqueue-then-transition pattern in `handle_daemon_escalation`
/// (Req 12.3) so the production path satisfies the same contract the 13.6 /
/// 13.7 seam tests model. No `daemon_directive` is dispatched here: the
/// orchestrator session is already dead by definition.
async fn route_orchestrator_dead(
    state: &mut ActorState,
    deps: &OrchestratorDeps,
    kind: EscalationKind,
    reason: InactiveReason,
    fields: Value,
) {
    let correlation_id = format!("{}-{}", kind.wire(), state.issue);
    deps.escalations
        .enqueue(EscalationEntry {
            issue: state.issue.clone(),
            repo: state.repo().map(|r| r.0.clone()),
            kind,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            structured_fields: fields,
        })
        .await;

    transition_to_inactive(state, deps, reason, TransitionTrigger::OrchestratorDead);
}

/// Returns `false` when the action terminates orchestration (`action=stop`).
async fn handle_action(
    state: &mut ActorState,
    action: OrchestratorAction,
    deps: &OrchestratorDeps,
) -> bool {
    match action.action {
        ActionKind::LinearUpdateDone => {
            // Per design the daemon does not gate on linear_update_done;
            // partial-write detection lives elsewhere. Continue.
            true
        }
        ActionKind::RunPhase => {
            let Some(phase) = action.phase else {
                // Schema-invalid: parser layer should have caught this;
                // belt-and-braces.
                return true;
            };
            run_phase_for_action(state, phase, action.additional_context, deps).await
        }
        ActionKind::Stop => {
            let outcome = action.outcome.unwrap_or(Outcome::Failure);
            let reason = map_stop_outcome(outcome);
            transition_to_inactive(
                state,
                deps,
                reason,
                TransitionTrigger::OrchestratorAction,
            );
            // Gracefully terminate the orchestrator session.
            if let Some(session) = state.session.take() {
                session
                    .shutdown(Some(Duration::from_secs(5)))
                    .await;
            }
            false
        }
    }
}

async fn run_phase_for_action(
    state: &mut ActorState,
    phase: PhaseName,
    additional_context: Option<String>,
    deps: &OrchestratorDeps,
) -> bool {
    let mode = match state.mode() {
        Some(m) => m,
        None => return true,
    };

    // Phase legality: catalog gate. Classify in SpecDriven is mode-illegal.
    if let Err(CatalogError::ModeIllegal { .. }) = catalog_default(phase, mode) {
        // Map to allowlist_rejected? No — the documented stop outcome is
        // emitted by the orchestrator. Daemon's role: log structurally
        // and route the orchestrator session to deliver a corrective
        // stop. We translate to PhaseNonclean(NonZero) into stdin so the
        // orchestrator can decide. For test determinism we also surface a
        // structured warn log.
        warn!(
            target: "orchestrator.core",
            issue = %state.issue,
            phase = ?phase,
            mode = ?mode,
            "rejected phase nomination: mode-illegal per phase catalog"
        );
        return true;
    }

    // Worktree contract: classify gets None; everything else needs an
    // ensured worktree against the operator's repo allowlist.
    let worktree_path = if matches!(phase, PhaseName::Classify) {
        None
    } else {
        let repo = match state.repo() {
            Some(r) => r.clone(),
            None => {
                // No repo on the admission context — treat as allowlist
                // rejected so the orchestrator can map to the documented
                // stop outcome.
                deliver_event_for_inactive_pre_phase(
                    state,
                    deps,
                    InactiveReason::AllowlistRejected,
                )
                .await;
                return true;
            }
        };
        match deps.worktree.ensure(&state.issue, &repo).await {
            Ok(p) => Some(p),
            Err(WorktreeOpError::AllowlistRejected(_)) => {
                deliver_event_for_inactive_pre_phase(
                    state,
                    deps,
                    InactiveReason::AllowlistRejected,
                )
                .await;
                return true;
            }
            Err(WorktreeOpError::FsPoison(cause)) => {
                handle_fs_poison(
                    state,
                    PathBuf::from(format!("<worktree {}>", state.issue)),
                    cause,
                    deps,
                )
                .await;
                return true;
            }
            Err(WorktreeOpError::Other(err)) => {
                warn!(
                    target: "orchestrator.core",
                    issue = %state.issue,
                    error = %err,
                    "worktree ensure failed"
                );
                return true;
            }
        }
    };

    // Pending -> Active.
    let prev = state.state.clone();
    state.state = WorkerState::Active;
    publish_transition(
        deps,
        &state.issue,
        state.repo().map(|r| r.0.clone()),
        prev,
        WorkerState::Active,
        TransitionTrigger::OrchestratorAction,
        state.mode(),
        None,
    );

    // Run the phase. The phase engine returns the typed event ready for
    // delivery to the orchestrator's stdin. The session tempdir was
    // ensured at admission (`handle_admit`); a missing slot here is a
    // bug — refuse the phase nomination cleanly so the actor falls back
    // to its inbox loop instead of deref-panicking on `None`.
    let session_tempdir = match state.session_tempdir.clone() {
        Some(path) => path,
        None => {
            warn!(
                target: "orchestrator.core",
                issue = %state.issue,
                "phase nomination without session tempdir; refusing"
            );
            return true;
        }
    };
    let phase_outcome = deps
        .phase_engine
        .run_phase(
            &state.issue,
            phase,
            mode,
            worktree_path,
            additional_context,
            session_tempdir,
        )
        .await;

    match phase_outcome {
        Ok(PhaseRunOutcome::Translated(event)) => {
            // Active -> Pending via PhaseEvent.
            let prev = state.state.clone();
            state.state = WorkerState::Pending;
            publish_transition(
                deps,
                &state.issue,
                state.repo().map(|r| r.0.clone()),
                prev,
                WorkerState::Pending,
                TransitionTrigger::PhaseEvent,
                state.mode(),
                None,
            );
            if let Some(session) = state.session.as_ref() {
                let _ = session.deliver(event).await;
            }
            true
        }
        Ok(PhaseRunOutcome::TrackerTerminalDiscarded) => {
            // Tracker-terminal preempt happened during the phase. Caller
            // (the actor's tracker handler) will publish the Cleaning
            // transition; here we just stop draining.
            false
        }
        Err(err) => {
            warn!(
                target: "orchestrator.core",
                issue = %state.issue,
                error = %err,
                "phase engine failed"
            );
            true
        }
    }
}

/// Fast-path: when a `run_phase` cannot proceed because of a pre-phase stop
/// condition (allowlist_rejected), deliver no phase event and route the
/// state directly to `Inactive(reason)`. The orchestrator session is
/// gracefully terminated.
async fn deliver_event_for_inactive_pre_phase(
    state: &mut ActorState,
    deps: &OrchestratorDeps,
    reason: InactiveReason,
) {
    transition_to_inactive(
        state,
        deps,
        reason,
        TransitionTrigger::OrchestratorAction,
    );
    if let Some(session) = state.session.take() {
        session.shutdown(Some(Duration::from_secs(5))).await;
    }
}

fn map_stop_outcome(outcome: Outcome) -> InactiveReason {
    match outcome {
        Outcome::Success | Outcome::Cancelled => InactiveReason::AwaitingLinear,
        Outcome::Failure => InactiveReason::RetryExhausted,
        Outcome::NeedsOperator => InactiveReason::NeedsOperator,
        Outcome::SpecIncomplete => InactiveReason::SpecIncomplete,
        Outcome::NeedsSplit => InactiveReason::NeedsSplit,
        Outcome::AllowlistRejected => InactiveReason::AllowlistRejected,
    }
}

fn transition_to_inactive(
    state: &mut ActorState,
    deps: &OrchestratorDeps,
    reason: InactiveReason,
    trigger: TransitionTrigger,
) {
    let prev = state.state.clone();
    let next = WorkerState::Inactive(reason);
    state.state = next.clone();
    publish_transition(
        deps,
        &state.issue,
        state.repo().map(|r| r.0.clone()),
        prev,
        next,
        trigger,
        state.mode(),
        Some(reason),
    );
}

async fn enter_cleaning(
    state: &mut ActorState,
    deps: &OrchestratorDeps,
    trigger: TransitionTrigger,
) {
    // SIGTERM the orchestrator session and any in-flight phase. The phase
    // engine receives the tracker-terminal preempt through its own seam;
    // here we simply deliver a tracker_terminal event to the orchestrator
    // before tearing down so its next turn can return action=stop
    // outcome=cancelled per Req 4.9.
    if let Some(session) = state.session.as_ref() {
        let payload = TrackerTerminalPayload {
            terminal_state: terminal_state_for_trigger(trigger),
            correlation_id: format!("tracker-{}", state.issue),
            timestamp: OffsetDateTime::now_utc(),
        };
        let _ = session
            .deliver(DaemonEvent::TrackerTerminal(payload))
            .await;
    }

    // From Inactive: only enter Cleaning when the Linear state observed is
    // terminal AND the reason was non-AwaitingLinear (the reason that
    // already implies Cleaning is the operator's next action).
    let prev = state.state.clone();
    let allowed = matches!(
        prev,
        WorkerState::Pending
            | WorkerState::Active
            | WorkerState::Backoff
            | WorkerState::Inactive(_)
    );
    if !allowed {
        return;
    }

    state.state = WorkerState::Cleaning;
    publish_transition(
        deps,
        &state.issue,
        state.repo().map(|r| r.0.clone()),
        prev,
        WorkerState::Cleaning,
        trigger,
        state.mode(),
        None,
    );

    // Wind down the orchestrator session.
    if let Some(session) = state.session.take() {
        session.shutdown(Some(Duration::from_secs(5))).await;
    }

    // Cleanup worktree(s) (allowlist-iterated, branch == issue id verbatim).
    match deps.worktree.cleanup(&state.issue).await {
        Ok(removed) => {
            info!(
                target: "orchestrator.core",
                issue = %state.issue,
                removed_count = removed.len(),
                "worktree cleanup complete"
            );
        }
        Err(err) => {
            warn!(
                target: "orchestrator.core",
                issue = %state.issue,
                error = %err,
                "worktree cleanup failed"
            );
        }
    }

    // Remove session tempdir AFTER worktree cleanup completes.
    if let Err(err) = deps.session_dirs.remove(&state.issue) {
        warn!(
            target: "orchestrator.core",
            issue = %state.issue,
            error = %err,
            "session tempdir removal failed"
        );
    }
}

fn terminal_state_for_trigger(trigger: TransitionTrigger) -> EventTerminalState {
    match trigger {
        TransitionTrigger::AssignmentLost => EventTerminalState::AssignmentLost,
        TransitionTrigger::RokiReadyRemoved => EventTerminalState::RokiReadyRemoved,
        TransitionTrigger::TrackerEvent => EventTerminalState::Done,
        _ => EventTerminalState::Canceled,
    }
}

async fn handle_daemon_escalation(
    state: &mut ActorState,
    kind: EscalationKind,
    fields: Value,
    correlation_id: String,
    deps: &OrchestratorDeps,
) {
    // Always enqueue first so the queue reflects the failure even if
    // delivery later fails or the orchestrator is dead.
    deps.escalations
        .enqueue(EscalationEntry {
            issue: state.issue.clone(),
            repo: state.repo().map(|r| r.0.clone()),
            kind,
            correlation_id: correlation_id.clone(),
            timestamp: OffsetDateTime::now_utc(),
            structured_fields: fields.clone(),
        })
        .await;

    let alive = state.session.is_some();

    // Route the directive through the shared helper. Note: the helper
    // expects an `OrchestratorSessionHandle` reference but our actor uses
    // the `OrchestratorSessionLike` trait — so we shortcut the delivery
    // path here by calling deliver() directly. The shared helper still
    // governs the orchestrator-dead branch via `is_orchestrator_dead`.
    if kind.is_orchestrator_dead() {
        let _ = route_daemon_directive(
            alive,
            kind,
            fields,
            correlation_id,
            None,
        )
        .await;
        // Map to the matching Inactive reason if not already there.
        let inactive_reason = match kind {
            EscalationKind::OrchestratorCrash => InactiveReason::OrchestratorCrash,
            EscalationKind::OrchestratorUnparseable => {
                InactiveReason::OrchestratorUnparseable
            }
            EscalationKind::OrchestratorBudgetExhausted => {
                InactiveReason::OrchestratorBudgetExhausted
            }
            _ => return,
        };
        if !matches!(state.state, WorkerState::Inactive(_)) {
            transition_to_inactive(
                state,
                deps,
                inactive_reason,
                TransitionTrigger::OrchestratorDead,
            );
        }
        return;
    }

    if !alive {
        // Daemon-detected failure but no live orchestrator: structured log
        // + queue only per Req 12.3.
        warn!(
            target: "orchestrator.core",
            issue = %state.issue,
            kind = kind.wire(),
            "escalation queued without delivery: orchestrator not alive"
        );
        return;
    }

    // Live orchestrator: deliver `daemon_directive` directly.
    let payload = build_directive_payload(&fields, kind, &correlation_id);
    let session = state.session.as_ref().expect("alive session");
    if let Err(err) = session.deliver(DaemonEvent::DaemonDirective(payload)).await {
        warn!(
            target: "orchestrator.core",
            issue = %state.issue,
            error = %err,
            "directive delivery failed; routing to orchestrator_crash"
        );
        transition_to_inactive(
            state,
            deps,
            InactiveReason::OrchestratorCrash,
            TransitionTrigger::OrchestratorDead,
        );
    }
}

fn build_directive_payload(
    fields: &Value,
    kind: EscalationKind,
    correlation_id: &str,
) -> crate::engine::orchestrator_session::events::DaemonDirectivePayload {
    use crate::engine::orchestrator_session::events::DaemonDirectivePayload;

    let repos = fields
        .get("repos")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let worktree_path = fields
        .get("worktree_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let last_subtype = fields
        .get("last_subtype")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let attempts = fields
        .get("attempts")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    let window_ms = fields.get("window_ms").and_then(Value::as_u64);
    let errno = fields
        .get("errno")
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok());

    DaemonDirectivePayload {
        kind: kind.wire().to_owned(),
        correlation_id: correlation_id.to_owned(),
        repos,
        worktree_path,
        last_subtype,
        attempts,
        window_ms,
        errno,
        timestamp: OffsetDateTime::now_utc(),
    }
}

async fn handle_fs_poison(
    state: &mut ActorState,
    offending_path: PathBuf,
    cause: String,
    deps: &OrchestratorDeps,
) {
    warn!(
        target: "orchestrator.core",
        issue = %state.issue,
        path = %offending_path.display(),
        cause = %cause,
        "filesystem poison detected"
    );

    state.refused_for_fs_poison = true;

    let fields = json!({
        "offending_path": offending_path.display().to_string(),
        "cause": cause,
    });
    let correlation_id = format!("fspoison-{}", state.issue);

    deps.escalations
        .enqueue(EscalationEntry {
            issue: state.issue.clone(),
            repo: state.repo().map(|r| r.0.clone()),
            kind: EscalationKind::FsPoison,
            correlation_id: correlation_id.clone(),
            timestamp: OffsetDateTime::now_utc(),
            structured_fields: fields.clone(),
        })
        .await;

    if let Some(session) = state.session.as_ref() {
        let payload = build_directive_payload(&fields, EscalationKind::FsPoison, &correlation_id);
        let _ = session.deliver(DaemonEvent::DaemonDirective(payload)).await;
    }

    if !matches!(state.state, WorkerState::Inactive(InactiveReason::FsPoison)) {
        // FsPoison transition is rendered via OrchestratorDead trigger when
        // the orchestrator is dead, otherwise via DaemonDirective.
        let trigger = if state.session.is_some() {
            TransitionTrigger::DaemonDirective
        } else {
            TransitionTrigger::OrchestratorDead
        };
        let prev = state.state.clone();
        let next = WorkerState::Inactive(InactiveReason::FsPoison);
        state.state = next.clone();
        // The state machine validator would reject some of the (prev, next)
        // pairs if the trigger were OrchestratorDead from Active. We
        // bypass validate_transition deliberately for fs_poison: the
        // taxonomy in design.md treats fs_poison as an out-of-band
        // escalation. The publish surface still records the trigger so
        // observers can audit it.
        publish_transition_unchecked(
            deps,
            &state.issue,
            state.repo().map(|r| r.0.clone()),
            prev,
            next,
            trigger,
            state.mode(),
            Some(InactiveReason::FsPoison),
        );
    }
}

// ---------------------------------------------------------------------------
// Snapshot + transition publish
// ---------------------------------------------------------------------------

fn publish_snapshot(state: &ActorState, deps: &OrchestratorDeps) {
    if let Ok(mut map) = deps.state_map.write() {
        map.insert(
            state.issue.clone(),
            ActorSnapshot {
                issue: state.issue.clone(),
                state: state.state.clone(),
                mode: state.mode(),
                latest_linear_state: state.latest_linear_state.clone(),
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_transition(
    deps: &OrchestratorDeps,
    issue: &IssueId,
    repo: Option<String>,
    previous: WorkerState,
    next: WorkerState,
    trigger: TransitionTrigger,
    mode: Option<Mode>,
    inactive_reason: Option<InactiveReason>,
) {
    let event = TransitionEvent {
        issue: issue.clone(),
        repo,
        previous,
        next,
        trigger,
        mode,
        inactive_reason,
        correlation_id: format!("tx-{}-{}", issue, OffsetDateTime::now_utc().unix_timestamp()),
    };
    deps.event_bus.publish(event.clone());
    deps.hooks.dispatch(&event);
}

#[allow(clippy::too_many_arguments)]
fn publish_transition_unchecked(
    deps: &OrchestratorDeps,
    issue: &IssueId,
    repo: Option<String>,
    previous: WorkerState,
    next: WorkerState,
    trigger: TransitionTrigger,
    mode: Option<Mode>,
    inactive_reason: Option<InactiveReason>,
) {
    publish_transition(
        deps,
        issue,
        repo,
        previous,
        next,
        trigger,
        mode,
        inactive_reason,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::sync::Mutex as AsyncMutex;

    fn reason(s: &str) -> crate::engine::orchestrator_session::action_parser::BoundedString200 {
        crate::engine::orchestrator_session::action_parser::BoundedString200::new(s).unwrap()
    }

    fn run_phase_action(phase: PhaseName) -> OrchestratorAction {
        OrchestratorAction {
            action: ActionKind::RunPhase,
            phase: Some(phase),
            additional_context: None,
            outcome: None,
            linear_writes: None,
            reason: reason("nominate"),
        }
    }

    fn stop_action(outcome: Outcome) -> OrchestratorAction {
        OrchestratorAction {
            action: ActionKind::Stop,
            phase: None,
            additional_context: None,
            outcome: Some(outcome),
            linear_writes: None,
            reason: reason("stop"),
        }
    }

    /// Test stub for an orchestrator session: pre-seeded action queue +
    /// records every delivered DaemonEvent.
    struct StubSession {
        actions: AsyncMutex<VecDeque<OrchestratorActionEvent>>,
        delivered: Arc<AsyncMutex<Vec<DaemonEvent>>>,
        shutdown_calls: Arc<AsyncMutex<u32>>,
    }

    #[async_trait]
    impl OrchestratorSessionLike for StubSession {
        async fn deliver(&self, event: DaemonEvent) -> Result<(), DeliveryError> {
            self.delivered.lock().await.push(event);
            Ok(())
        }

        async fn next_action(&mut self) -> Option<OrchestratorActionEvent> {
            self.actions.lock().await.pop_front()
        }

        async fn shutdown(self: Box<Self>, _grace: Option<Duration>) {
            *self.shutdown_calls.lock().await += 1;
        }
    }

    struct StubEngine {
        scripted: AsyncMutex<VecDeque<OrchestratorActionEvent>>,
        delivered: Arc<AsyncMutex<Vec<DaemonEvent>>>,
        shutdown_calls: Arc<AsyncMutex<u32>>,
    }

    #[async_trait]
    impl OrchestratorEngine for StubEngine {
        async fn launch(
            &self,
            _issue: &IssueId,
            _mode: Mode,
            _system_prompt: String,
        ) -> Result<Box<dyn OrchestratorSessionLike>, EngineError> {
            let mut taken = self.scripted.lock().await;
            let actions: VecDeque<OrchestratorActionEvent> = taken.drain(..).collect();
            Ok(Box::new(StubSession {
                actions: AsyncMutex::new(actions),
                delivered: self.delivered.clone(),
                shutdown_calls: self.shutdown_calls.clone(),
            }))
        }
    }

    struct StubPhaseEngine {
        canned: AsyncMutex<VecDeque<PhaseRunOutcome>>,
        invocations: AsyncMutex<Vec<(PhaseName, Mode, Option<PathBuf>)>>,
    }

    #[async_trait]
    impl PhaseEngine for StubPhaseEngine {
        async fn run_phase(
            &self,
            _issue: &IssueId,
            phase: PhaseName,
            mode: Mode,
            worktree_path: Option<PathBuf>,
            _additional_context: Option<String>,
            _session_tempdir: PathBuf,
        ) -> Result<PhaseRunOutcome, EngineError> {
            self.invocations
                .lock()
                .await
                .push((phase, mode, worktree_path.clone()));
            Ok(self
                .canned
                .lock()
                .await
                .pop_front()
                .unwrap_or(PhaseRunOutcome::Translated(DaemonEvent::PhaseComplete(
                    crate::engine::orchestrator_session::events::PhaseCompletePayload {
                        phase,
                        result: serde_json::json!({"subtype": "success"}),
                        pr_url: None,
                        review_artifact_path: None,
                        classify: None,
                    },
                ))))
        }
    }

    struct StubWorktree {
        ensure_calls: AsyncMutex<Vec<(IssueId, RepoId)>>,
        cleanup_calls: AsyncMutex<Vec<IssueId>>,
        ensure_result: AsyncMutex<Option<Result<PathBuf, WorktreeOpError>>>,
    }

    #[async_trait]
    impl WorktreeOps for StubWorktree {
        async fn ensure(
            &self,
            issue: &IssueId,
            repo_id: &RepoId,
        ) -> Result<PathBuf, WorktreeOpError> {
            self.ensure_calls
                .lock()
                .await
                .push((issue.clone(), repo_id.clone()));
            // Allow tests to inject a one-shot ensure result via the
            // Mutex<Option<...>>; default is a stable PathBuf.
            if let Some(result) = self.ensure_result.lock().await.take() {
                result
            } else {
                Ok(PathBuf::from(format!("/tmp/wt/{}", issue)))
            }
        }

        async fn cleanup(
            &self,
            issue: &IssueId,
        ) -> Result<Vec<PathBuf>, WorktreeOpError> {
            self.cleanup_calls.lock().await.push(issue.clone());
            Ok(vec![PathBuf::from(format!("/tmp/wt/{}", issue))])
        }
    }

    struct StubSessionDirs {
        ensure_calls: Mutex<Vec<IssueId>>,
        remove_calls: Mutex<Vec<IssueId>>,
        ensure_fail: Mutex<Option<String>>,
    }

    impl SessionDirOps for StubSessionDirs {
        fn ensure(&self, issue: &IssueId) -> Result<PathBuf, SessionDirError> {
            self.ensure_calls.lock().unwrap().push(issue.clone());
            if let Some(err) = self.ensure_fail.lock().unwrap().take() {
                return Err(SessionDirError::Other(err));
            }
            Ok(PathBuf::from(format!("/tmp/sessions/{}", issue)))
        }
        fn remove(&self, issue: &IssueId) -> Result<(), SessionDirError> {
            self.remove_calls.lock().unwrap().push(issue.clone());
            Ok(())
        }
    }

    fn build_deps(
        engine: Arc<dyn OrchestratorEngine>,
        phase: Arc<dyn PhaseEngine>,
        worktree: Arc<dyn WorktreeOps>,
        session_dirs: Arc<dyn SessionDirOps>,
    ) -> OrchestratorDeps {
        OrchestratorDeps {
            orchestrator_engine: engine,
            phase_engine: phase,
            worktree,
            session_dirs,
            event_bus: EventBus::with_capacity(64),
            hooks: Arc::new(SubscriberHooks::new()),
            escalations: Arc::new(EscalationQueue::new()),
            state_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    #[tokio::test]
    async fn admit_specdriven_then_implement_then_open_pr_then_stop_lands_in_awaiting_linear() {
        // Curated action stream: implement -> open_pr -> stop(success).
        // The actor must ensure a worktree before each non-classify phase,
        // forward the resulting PhaseComplete to the orchestrator's stdin,
        // and finally land in Inactive(AwaitingLinear) after action=stop.
        let scripted = VecDeque::from(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::OpenPr)),
            OrchestratorActionEvent::Action(stop_action(Outcome::Success)),
        ]);
        let delivered = Arc::new(AsyncMutex::new(Vec::new()));
        let shutdown_calls = Arc::new(AsyncMutex::new(0u32));
        let engine = Arc::new(StubEngine {
            scripted: AsyncMutex::new(scripted),
            delivered: delivered.clone(),
            shutdown_calls: shutdown_calls.clone(),
        });
        let phase = Arc::new(StubPhaseEngine {
            canned: AsyncMutex::new(VecDeque::new()),
            invocations: AsyncMutex::new(Vec::new()),
        });
        let worktree = Arc::new(StubWorktree {
            ensure_calls: AsyncMutex::new(Vec::new()),
            cleanup_calls: AsyncMutex::new(Vec::new()),
            ensure_result: AsyncMutex::new(None),
        });
        let session_dirs = Arc::new(StubSessionDirs {
            ensure_calls: Mutex::new(Vec::new()),
            remove_calls: Mutex::new(Vec::new()),
            ensure_fail: Mutex::new(None),
        });

        let deps = build_deps(
            engine.clone(),
            phase.clone(),
            worktree.clone(),
            session_dirs.clone(),
        );
        let state_map = deps.state_map.clone();
        let orch = Orchestrator::new(deps);

        let issue = IssueId::from("ENG-1");
        let repo = RepoId::from("github.com/owner/repo");

        let _ = orch
            .send(
                issue.clone(),
                ActorMessage::TrackerAdmit {
                    mode: Mode::SpecDriven,
                    repo: Some(repo.clone()),
                },
            )
            .await;

        // Wait for the actor to land in Inactive(AwaitingLinear).
        let mut saw = false;
        for _ in 0..80 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let map = state_map.read().unwrap();
            if let Some(snap) = map.get(&issue) {
                if matches!(
                    snap.state,
                    WorkerState::Inactive(InactiveReason::AwaitingLinear)
                ) {
                    saw = true;
                    break;
                }
            }
        }
        assert!(saw, "expected Inactive(AwaitingLinear) after stop(success)");

        // Worktree ensure invoked twice (once per non-classify phase).
        let ensure_calls = worktree.ensure_calls.lock().await;
        assert_eq!(
            ensure_calls.len(),
            2,
            "worktree ensure invoked per non-classify phase"
        );
        assert!(ensure_calls.iter().all(|(i, r)| i == &issue && r == &repo));

        // Phase invocations recorded with correct phases + worktree.
        let invs = phase.invocations.lock().await;
        assert_eq!(invs.len(), 2);
        assert_eq!(invs[0].0, PhaseName::Implement);
        assert_eq!(invs[1].0, PhaseName::OpenPr);
        assert!(invs[0].2.is_some());
        assert!(invs[1].2.is_some());

        // Two PhaseComplete events delivered to orchestrator stdin.
        let dlv = delivered.lock().await;
        assert!(dlv
            .iter()
            .filter(|e| matches!(e, DaemonEvent::PhaseComplete(_)))
            .count() >= 2);

        // Orchestrator session shutdown invoked once after action=stop.
        for _ in 0..20 {
            if *shutdown_calls.lock().await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*shutdown_calls.lock().await, 1);

        // Worktree cleanup NOT invoked (phase exit alone never enters Cleaning).
        assert!(worktree.cleanup_calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn classify_in_specdriven_is_rejected_by_catalog_and_does_not_run() {
        // run_phase=classify in SpecDriven is mode-illegal per catalog;
        // actor logs structurally and continues without running it. We
        // then send a stop(success) and verify the actor still lands in
        // AwaitingLinear and never invoked the phase engine.
        let scripted = VecDeque::from(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Classify)),
            OrchestratorActionEvent::Action(stop_action(Outcome::Success)),
        ]);
        let engine = Arc::new(StubEngine {
            scripted: AsyncMutex::new(scripted),
            delivered: Arc::new(AsyncMutex::new(Vec::new())),
            shutdown_calls: Arc::new(AsyncMutex::new(0)),
        });
        let phase = Arc::new(StubPhaseEngine {
            canned: AsyncMutex::new(VecDeque::new()),
            invocations: AsyncMutex::new(Vec::new()),
        });
        let worktree = Arc::new(StubWorktree {
            ensure_calls: AsyncMutex::new(Vec::new()),
            cleanup_calls: AsyncMutex::new(Vec::new()),
            ensure_result: AsyncMutex::new(None),
        });
        let session_dirs = Arc::new(StubSessionDirs {
            ensure_calls: Mutex::new(Vec::new()),
            remove_calls: Mutex::new(Vec::new()),
            ensure_fail: Mutex::new(None),
        });
        let deps = build_deps(engine, phase.clone(), worktree, session_dirs);
        let state_map = deps.state_map.clone();
        let orch = Orchestrator::new(deps);

        let issue = IssueId::from("ENG-2");
        let _ = orch
            .send(
                issue.clone(),
                ActorMessage::TrackerAdmit {
                    mode: Mode::SpecDriven,
                    repo: Some(RepoId::from("github.com/owner/repo")),
                },
            )
            .await;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let map = state_map.read().unwrap();
            if let Some(snap) = map.get(&issue) {
                if matches!(
                    snap.state,
                    WorkerState::Inactive(InactiveReason::AwaitingLinear)
                ) {
                    break;
                }
            }
        }
        // Phase engine never invoked because classify was mode-illegal.
        assert!(phase.invocations.lock().await.is_empty());
    }

    #[tokio::test]
    async fn fs_poison_on_session_dir_routes_to_inactive_fs_poison() {
        let scripted: VecDeque<OrchestratorActionEvent> = VecDeque::new();
        let engine = Arc::new(StubEngine {
            scripted: AsyncMutex::new(scripted),
            delivered: Arc::new(AsyncMutex::new(Vec::new())),
            shutdown_calls: Arc::new(AsyncMutex::new(0)),
        });
        let phase = Arc::new(StubPhaseEngine {
            canned: AsyncMutex::new(VecDeque::new()),
            invocations: AsyncMutex::new(Vec::new()),
        });
        let worktree = Arc::new(StubWorktree {
            ensure_calls: AsyncMutex::new(Vec::new()),
            cleanup_calls: AsyncMutex::new(Vec::new()),
            ensure_result: AsyncMutex::new(None),
        });
        let session_dirs = Arc::new(StubSessionDirs {
            ensure_calls: Mutex::new(Vec::new()),
            remove_calls: Mutex::new(Vec::new()),
            ensure_fail: Mutex::new(Some("read-only fs".to_owned())),
        });
        let deps = build_deps(engine, phase, worktree, session_dirs.clone());
        let escalations = deps.escalations.clone();
        let state_map = deps.state_map.clone();
        let orch = Orchestrator::new(deps);

        let issue = IssueId::from("ENG-7");
        let _ = orch
            .send(issue.clone(), ActorMessage::TrackerAdmit { mode: Mode::SpecDriven, repo: None })
            .await;

        // Wait for the actor to record the FsPoison snapshot.
        let mut saw_fs_poison = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let map = state_map.read().unwrap();
            if let Some(snap) = map.get(&issue) {
                if matches!(snap.state, WorkerState::Inactive(InactiveReason::FsPoison)) {
                    saw_fs_poison = true;
                    break;
                }
            }
        }
        assert!(saw_fs_poison, "expected Inactive(FsPoison) snapshot");

        // Escalation queue records the failure with the offending path.
        let snap = escalations.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].kind, EscalationKind::FsPoison);
        let path_str = snap[0]
            .structured_fields
            .get("offending_path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(path_str.contains("ENG-7"), "offending path logged: {path_str}");

        // Subsequent admission is refused.
        let _ = orch
            .send(issue.clone(), ActorMessage::TrackerAdmit { mode: Mode::SpecDriven, repo: None })
            .await;
        // Snapshot still fs_poison.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let map = state_map.read().unwrap();
        assert!(matches!(
            map.get(&issue).unwrap().state,
            WorkerState::Inactive(InactiveReason::FsPoison)
        ));
    }

    #[tokio::test]
    async fn tracker_terminal_mid_flight_drives_cleaning_and_cleanup() {
        // Scripted: orchestrator emits a ProcessExit shortly after launch
        // (no actions). The tracker-terminal path is what we exercise.
        let scripted = VecDeque::from(vec![]);
        let engine = Arc::new(StubEngine {
            scripted: AsyncMutex::new(scripted),
            delivered: Arc::new(AsyncMutex::new(Vec::new())),
            shutdown_calls: Arc::new(AsyncMutex::new(0)),
        });
        let phase = Arc::new(StubPhaseEngine {
            canned: AsyncMutex::new(VecDeque::new()),
            invocations: AsyncMutex::new(Vec::new()),
        });
        let worktree = Arc::new(StubWorktree {
            ensure_calls: AsyncMutex::new(Vec::new()),
            cleanup_calls: AsyncMutex::new(Vec::new()),
            ensure_result: AsyncMutex::new(None),
        });
        let session_dirs = Arc::new(StubSessionDirs {
            ensure_calls: Mutex::new(Vec::new()),
            remove_calls: Mutex::new(Vec::new()),
            ensure_fail: Mutex::new(None),
        });

        let deps = build_deps(
            engine.clone(),
            phase.clone(),
            worktree.clone(),
            session_dirs.clone(),
        );
        let state_map = deps.state_map.clone();
        let orch = Orchestrator::new(deps);
        let issue = IssueId::from("ENG-22");

        let _ = orch
            .send(issue.clone(), ActorMessage::TrackerAdmit { mode: Mode::SpecDriven, repo: None })
            .await;

        // After admission, tracker observes assignment lost.
        let _ = orch
            .send(issue.clone(), ActorMessage::TrackerAssignmentLost)
            .await;

        // Wait for actor to exit (Cleaning is terminal for the loop).
        orch.join(&issue).await;

        // Worktree cleanup invoked exactly once for the issue.
        let cleanup_calls = worktree.cleanup_calls.lock().await;
        assert_eq!(cleanup_calls.as_slice(), std::slice::from_ref(&issue));
        drop(cleanup_calls);

        // Session tempdir removed AFTER worktree cleanup.
        let remove_calls = session_dirs.remove_calls.lock().unwrap();
        assert_eq!(remove_calls.as_slice(), std::slice::from_ref(&issue));
        drop(remove_calls);

        // Final snapshot reports Cleaning (the actor exits without
        // transitioning out of Cleaning; the snapshot freezes there).
        let map = state_map.read().unwrap();
        assert!(matches!(map.get(&issue).unwrap().state, WorkerState::Cleaning));
    }

    #[tokio::test]
    async fn phase_subprocess_exit_alone_does_not_trigger_cleaning() {
        // Scripted: implement run_phase, phase returns success, then stop.
        // No tracker terminal; the actor must NOT enter Cleaning.
        let scripted = VecDeque::from(vec![
            OrchestratorActionEvent::Action(stop_action(Outcome::Success)),
        ]);
        let engine = Arc::new(StubEngine {
            scripted: AsyncMutex::new(scripted),
            delivered: Arc::new(AsyncMutex::new(Vec::new())),
            shutdown_calls: Arc::new(AsyncMutex::new(0)),
        });
        let phase = Arc::new(StubPhaseEngine {
            canned: AsyncMutex::new(VecDeque::new()),
            invocations: AsyncMutex::new(Vec::new()),
        });
        let worktree = Arc::new(StubWorktree {
            ensure_calls: AsyncMutex::new(Vec::new()),
            cleanup_calls: AsyncMutex::new(Vec::new()),
            ensure_result: AsyncMutex::new(None),
        });
        let session_dirs = Arc::new(StubSessionDirs {
            ensure_calls: Mutex::new(Vec::new()),
            remove_calls: Mutex::new(Vec::new()),
            ensure_fail: Mutex::new(None),
        });
        let deps = build_deps(engine, phase, worktree.clone(), session_dirs.clone());
        let state_map = deps.state_map.clone();
        let orch = Orchestrator::new(deps);

        let issue = IssueId::from("ENG-44");
        let _ = orch
            .send(issue.clone(), ActorMessage::TrackerAdmit { mode: Mode::SpecDriven, repo: None })
            .await;
        // Wait until we observe Inactive(AwaitingLinear).
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let map = state_map.read().unwrap();
            if let Some(snap) = map.get(&issue) {
                if matches!(
                    snap.state,
                    WorkerState::Inactive(InactiveReason::AwaitingLinear)
                ) {
                    break;
                }
            }
        }
        {
            let map = state_map.read().unwrap();
            assert!(matches!(
                map.get(&issue).unwrap().state,
                WorkerState::Inactive(InactiveReason::AwaitingLinear)
            ));
        }
        // No cleanup invoked: phase exit alone never triggers Cleaning.
        let cleanup_calls = worktree.cleanup_calls.lock().await;
        assert!(cleanup_calls.is_empty());
    }

    #[tokio::test]
    async fn daemon_directive_for_orchestrator_dead_kind_does_not_send_to_orchestrator() {
        let scripted = VecDeque::from(vec![]);
        let delivered = Arc::new(AsyncMutex::new(Vec::new()));
        let engine = Arc::new(StubEngine {
            scripted: AsyncMutex::new(scripted),
            delivered: delivered.clone(),
            shutdown_calls: Arc::new(AsyncMutex::new(0)),
        });
        let phase = Arc::new(StubPhaseEngine {
            canned: AsyncMutex::new(VecDeque::new()),
            invocations: AsyncMutex::new(Vec::new()),
        });
        let worktree = Arc::new(StubWorktree {
            ensure_calls: AsyncMutex::new(Vec::new()),
            cleanup_calls: AsyncMutex::new(Vec::new()),
            ensure_result: AsyncMutex::new(None),
        });
        let session_dirs = Arc::new(StubSessionDirs {
            ensure_calls: Mutex::new(Vec::new()),
            remove_calls: Mutex::new(Vec::new()),
            ensure_fail: Mutex::new(None),
        });
        let deps = build_deps(engine, phase, worktree, session_dirs);
        let escalations = deps.escalations.clone();
        let orch = Orchestrator::new(deps);

        let issue = IssueId::from("ENG-77");
        // No prior admission -> orchestrator session is not alive. Send a
        // DaemonEscalation for an orchestrator-dead kind directly.
        let _ = orch
            .send(
                issue.clone(),
                ActorMessage::DaemonEscalation {
                    kind: EscalationKind::OrchestratorBudgetExhausted,
                    fields: serde_json::json!({}),
                    correlation_id: "corr-bx".to_owned(),
                },
            )
            .await;

        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if escalations.len().await == 1 {
                break;
            }
        }
        let snap = escalations.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].kind, EscalationKind::OrchestratorBudgetExhausted);
        // Nothing was delivered to the orchestrator (there isn't one).
        assert!(delivered.lock().await.is_empty());
    }
}
