//! Production wiring of the phase-subprocess seam.
//!
//! Task 10.1.2 composes the orchestrator actor map but defers the
//! production phase pipeline to Task 10.1.5 (admission pipe). Wiring
//! [`PhaseSubprocessAdapter::spawn`] through the
//! [`crate::orchestrator::core::PhaseEngine`] trait requires building a
//! [`crate::engine::phase_subprocess::catalog::PhaseLaunchContext`] (workflow
//! policy, allowed-tools resolution, max-turns budget, session tempdir,
//! debug sink) plus exit translation through
//! [`crate::engine::phase_subprocess::exit::translate_exit`]; that
//! collaboration belongs alongside the admission pipe rather than being
//! force-fitted here as a side effect of the actor-map composition step.
//!
//! Until 10.1.5 lands, [`PendingPhaseEngine`] is the placeholder the
//! production runtime composition uses. It refuses every `run_phase` call
//! with [`EngineError::Internal`] so a `run_phase` arriving at this layer
//! before the production wiring is finished produces a clear, structured
//! error rather than a silent miswiring. The orchestrator core's actor loop
//! treats the error as a benign warn and stays in the `Pending` state — no
//! transition is published, and the actor remains drainable.
//!
//! Tests use the `StubPhaseEngine` from `tests/common/mod.rs` instead of
//! this placeholder; the placeholder only ships through
//! `runtime::run_with_shutdown`.
//!
//! Spec refs: requirements.md Req 5.6, 13.4; design.md "PhaseSubprocessAdapter".

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

use crate::engine::orchestrator_session::action_parser::PhaseName;
use crate::logging::DebugSinkFactory;
use crate::orchestrator::core::{EngineError, PhaseEngine, PhaseRunOutcome};
use crate::orchestrator::state::{IssueId, Mode};

/// Placeholder [`PhaseEngine`] surfaced by [`crate::runtime::run_with_shutdown`]
/// until Task 10.1.5 wires the admission pipe + production phase pipeline.
///
/// Holds the optional [`DebugSinkFactory`] composed by the runtime so the
/// production phase pipeline (Task 10.1.5) can route per-issue debug capture
/// without re-walking `RuntimeComponents`. The placeholder itself never
/// spawns, so the factory is unused until 10.1.5 lands.
#[derive(Debug, Default)]
pub struct PendingPhaseEngine {
    #[allow(dead_code)]
    debug_sink_factory: Option<Arc<DebugSinkFactory>>,
}

impl PendingPhaseEngine {
    pub fn new(debug_sink_factory: Option<Arc<DebugSinkFactory>>) -> Self {
        Self { debug_sink_factory }
    }
}

#[async_trait]
impl PhaseEngine for PendingPhaseEngine {
    async fn run_phase(
        &self,
        _issue: &IssueId,
        _phase: PhaseName,
        _mode: Mode,
        _worktree_path: Option<PathBuf>,
        _additional_context: Option<String>,
    ) -> Result<PhaseRunOutcome, EngineError> {
        Err(EngineError::Internal(
            "phase pipeline not yet wired into runtime composition; \
             tracked for Task 10.1.5 admission pipe"
                .to_owned(),
        ))
    }
}
