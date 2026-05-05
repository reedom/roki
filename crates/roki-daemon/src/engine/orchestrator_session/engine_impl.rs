//! Production wiring of [`OrchestratorSessionAdapter`] to the orchestrator
//! core's [`OrchestratorEngine`] seam.
//!
//! Why this lives in a separate module: the trait expects a self-contained
//! `launch(&issue, mode, system_prompt)` call, but the underlying adapter
//! also needs the per-issue session tempdir and the resolved orchestrator
//! `allowed_tools` list. [`OrchestratorEngineImpl`] composes those
//! collaborators so the trait stays narrow while the adapter's concrete
//! API is unchanged.
//!
//! The wrapper is pure routing: it never inspects the action stream or
//! mutates the adapter's state. The trait-side
//! [`OrchestratorSessionLike`] handle is implemented by
//! [`SessionLikeHandle`], which wraps the adapter's
//! [`OrchestratorSessionHandle`] and translates [`ActionEvent`] into the
//! orchestrator core's [`OrchestratorActionEvent`].
//!
//! Spec refs: requirements.md Req 4.1, 5.1, 7.1, 7.3, 13.1, 13.2; design.md
//! "Daemon bootstrap" step 8.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::engine::orchestrator_session::adapter::{
    ActionEvent, OrchestratorLaunchContext, OrchestratorSessionAdapter, OrchestratorSessionHandle,
};
use crate::engine::orchestrator_session::events::DaemonEvent;
use crate::orchestrator::core::{
    DeliveryError, EngineError, OrchestratorActionEvent, OrchestratorEngine,
    OrchestratorSessionLike,
};
use crate::orchestrator::state::{IssueId, Mode};
use crate::session::SessionManager;

/// Production [`OrchestratorEngine`] wired around the long-lived
/// [`OrchestratorSessionAdapter`]. Holds the additional collaborators the
/// adapter needs to assemble an [`OrchestratorLaunchContext`].
pub struct OrchestratorEngineImpl {
    adapter: Arc<OrchestratorSessionAdapter>,
    session_manager: Arc<SessionManager>,
    allowed_tools: Vec<String>,
}

impl OrchestratorEngineImpl {
    pub fn new(
        adapter: Arc<OrchestratorSessionAdapter>,
        session_manager: Arc<SessionManager>,
        allowed_tools: Vec<String>,
    ) -> Self {
        Self {
            adapter,
            session_manager,
            allowed_tools,
        }
    }
}

#[async_trait]
impl OrchestratorEngine for OrchestratorEngineImpl {
    async fn launch(
        &self,
        issue: &IssueId,
        mode: Mode,
        system_prompt: String,
    ) -> Result<Box<dyn OrchestratorSessionLike>, EngineError> {
        // The orchestrator core has already ensured the session tempdir via
        // its `SessionDirOps` seam before calling `launch` (see
        // `orchestrator::core::handle_admit`). We re-look it up here rather
        // than threading the path through the seam signature so the trait
        // stays narrow.
        let session_tempdir = self
            .session_manager
            .ensure(issue)
            .map_err(|err| EngineError::Internal(err.to_string()))?;

        let ctx = OrchestratorLaunchContext {
            issue: issue.clone(),
            mode,
            session_tempdir,
            system_prompt,
            allowed_tools: self.allowed_tools.clone(),
            // Per-issue debug sinks land alongside the logging crate; not
            // wired in this composition step (Req 11.5 owner is later in
            // the runtime composition).
            debug_sink: None,
        };
        let handle = self
            .adapter
            .launch(ctx)
            .await
            .map_err(|err| EngineError::LaunchFailed(err.to_string()))?;
        Ok(Box::new(SessionLikeHandle { inner: handle }))
    }
}

/// Bridge from the adapter's [`OrchestratorSessionHandle`] to the
/// orchestrator core's [`OrchestratorSessionLike`] trait. Translates
/// [`ActionEvent`] -> [`OrchestratorActionEvent`].
struct SessionLikeHandle {
    inner: OrchestratorSessionHandle,
}

#[async_trait]
impl OrchestratorSessionLike for SessionLikeHandle {
    async fn deliver(&self, event: DaemonEvent) -> Result<(), DeliveryError> {
        self.inner
            .stdin_tx
            .send(event)
            .await
            .map_err(|_| DeliveryError::Closed)
    }

    async fn next_action(&mut self) -> Option<OrchestratorActionEvent> {
        loop {
            match self.inner.action_rx.recv().await? {
                ActionEvent::Action(action) => {
                    return Some(OrchestratorActionEvent::Action(action));
                }
                ActionEvent::Drift { reprompt: _ } => {
                    // First-time drift is an internal reprompt: the parser
                    // will re-emit on the next turn. Loop and wait for the
                    // next event without surfacing to the orchestrator core.
                    continue;
                }
                ActionEvent::TerminalDrift { raw_stdout: _ } => {
                    return Some(OrchestratorActionEvent::TerminalDrift);
                }
                ActionEvent::ProcessExit { status, raw_stdout: _ } => {
                    return Some(OrchestratorActionEvent::ProcessExit {
                        success: status.success(),
                    });
                }
            }
        }
    }

    async fn shutdown(self: Box<Self>, grace: Option<Duration>) {
        let _status = self.inner.shutdown(grace).await;
    }
}
