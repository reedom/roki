#![allow(dead_code)]
//! Phase executor. Filled in Task 10.

use async_trait::async_trait;

use crate::error::PhaseInfraError;

use super::context::PhaseContext;
use super::outcome::{PhaseBody, PhaseKind, PhaseOutcome};

/// Trait the cycle uses to invoke phases. The production implementation is
/// `CommandPhaseExecutor`; tests substitute a deterministic fake.
#[async_trait]
pub trait PhaseExecutor: Send + Sync {
    async fn execute(
        &self,
        kind: PhaseKind,
        body: &PhaseBody,
        ctx: &PhaseContext,
        iter_dir: &std::path::Path,
    ) -> Result<PhaseOutcome, PhaseInfraError>;
}

/// Production phase executor. Implementation lives in Task 10.
pub struct CommandPhaseExecutor;
