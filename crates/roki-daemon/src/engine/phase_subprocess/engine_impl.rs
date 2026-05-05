//! Production wiring of the phase-subprocess seam.
//!
//! [`PhaseSubprocessEngineImpl`] is the production [`PhaseEngine`] surfaced
//! by [`crate::runtime::run_with_shutdown`]. It composes
//! [`PhaseSubprocessAdapter::spawn`] with
//! [`crate::engine::phase_subprocess::exit::translate_exit`] so the
//! orchestrator core's `phase_engine.run_phase(...)` reaches the real
//! subprocess launch + typed exit translation.
//!
//! Tracker-terminal preempt is delivered via the
//! orchestrator-session adapter (see [`crate::orchestrator::core::enter_cleaning`]);
//! this engine impl wires a never-firing oneshot into [`translate_exit`]
//! so the in-flight phase wins the race against a cancelled signal sender.
//! Mid-phase abort (the actor task being aborted while a phase is in flight)
//! is closed by the [`tokio::process::Command::kill_on_drop`] flag set in
//! [`crate::engine::claude::ClaudeSpawn::spawn`]: when the phase handle is
//! dropped during abort, the spawned `Child` SIGKILLs cleanly without
//! relying on the tracker-terminal seam.
//!
//! Spec refs: requirements.md Req 1.4, 5.6, 6.7, 7.1, 7.3, 13.4;
//! design.md "PhaseSubprocessAdapter".

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::engine::orchestrator_session::action_parser::PhaseName;
use crate::engine::phase_subprocess::adapter::PhaseSubprocessAdapter;
use crate::engine::phase_subprocess::catalog::PhaseLaunchContext;
use crate::engine::phase_subprocess::exit::{
    ExitOutcome, ExitTranslationInputs, translate_exit,
};
use crate::logging::DebugSinkFactory;
use crate::orchestrator::core::{EngineError, PhaseEngine, PhaseRunOutcome};
use crate::orchestrator::state::{IssueId, Mode};
use crate::permissions::PermissionResolver;
use crate::workflow::schema::WorkflowPolicy;

/// Production [`PhaseEngine`] wired around the bounded
/// [`PhaseSubprocessAdapter`]. Holds the additional collaborators the
/// adapter needs to assemble a [`PhaseLaunchContext`] (workflow policy,
/// permission resolver) plus the optional per-issue debug-sink factory the
/// stdout/stderr drainers feed.
pub struct PhaseSubprocessEngineImpl {
    adapter: Arc<PhaseSubprocessAdapter>,
    workflow_policy: Arc<WorkflowPolicy>,
    permissions: PermissionResolver,
    debug_sink_factory: Option<Arc<DebugSinkFactory>>,
}

impl PhaseSubprocessEngineImpl {
    pub fn new(
        adapter: Arc<PhaseSubprocessAdapter>,
        workflow_policy: Arc<WorkflowPolicy>,
        permissions: PermissionResolver,
        debug_sink_factory: Option<Arc<DebugSinkFactory>>,
    ) -> Self {
        Self {
            adapter,
            workflow_policy,
            permissions,
            debug_sink_factory,
        }
    }
}

#[async_trait]
impl PhaseEngine for PhaseSubprocessEngineImpl {
    async fn run_phase(
        &self,
        issue: &IssueId,
        phase: PhaseName,
        mode: Mode,
        worktree_path: Option<PathBuf>,
        additional_context: Option<String>,
        session_tempdir: PathBuf,
    ) -> Result<PhaseRunOutcome, EngineError> {
        // Reflect the resolved per-phase strategy + allowlist into the
        // launch context. `build_invocation` re-resolves through the
        // adapter's own resolver, so these fields are informational; we
        // populate them so structured logs and future readers see the
        // documented shape.
        let resolved = self
            .permissions
            .resolve_for_phase(phase)
            .map_err(|err| EngineError::Internal(err.to_string()))?;
        let allowed_tools = resolved.allowed_tools.clone().unwrap_or_default();
        let permission_strategy = resolved.strategy.clone();

        let ctx = PhaseLaunchContext {
            issue: issue.clone(),
            phase,
            mode,
            additional_context,
            worktree_path,
            session_tempdir,
            // The adapter resolves the effective `--max-turns` from the
            // catalog + override layer; the ctx field stays at 0 so a
            // stale value cannot mask the catalog default.
            max_turns: 0,
            workflow_policy: self.workflow_policy.clone(),
            permission_strategy,
            allowed_tools,
        };

        // Materialize the per-issue debug sink (when configured) so the
        // adapter's stdout/stderr drainers append into `<dir>/<issue>.log`
        // per Req 11.6 / 11.7.
        let debug_sink = self
            .debug_sink_factory
            .as_ref()
            .map(|factory| factory.for_issue(issue.0.as_str()));

        let handle = self
            .adapter
            .spawn(ctx, debug_sink)
            .await
            .map_err(|err| EngineError::LaunchFailed(err.to_string()))?;

        // No tracker-terminal preempt is plumbed through `PhaseEngine` yet;
        // the orchestrator core delivers the `tracker_terminal` event via
        // the orchestrator session and SIGKILLs the in-flight phase via
        // `kill_on_drop` when the actor aborts. The never-firing oneshot
        // ensures `translate_exit` cannot select the tracker-terminal arm.
        let (_send_tt, recv_tt) = oneshot::channel();

        let stall_window = Duration::from_secs(handle.stall_seconds.into());
        let inputs = ExitTranslationInputs {
            child: handle.child,
            stream_rx: handle.stream_rx,
            phase,
            stall_window,
            tracker_terminal_signal: recv_tt,
        };

        match translate_exit(inputs).await {
            ExitOutcome::Translated(event) => Ok(PhaseRunOutcome::Translated(event)),
            ExitOutcome::TrackerTerminalSolo(_) => Ok(PhaseRunOutcome::TrackerTerminalDiscarded),
        }
    }
}
