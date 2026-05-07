#![allow(dead_code)]
//! Cycle driver. Filled in Task 11.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed {
        kind: super::outcome::FailureKind,
        iter: u32,
    },
}

// `run_cycle` body lands in Task 11. Stub re-export so `engine::mod` compiles.
pub use stub::run_cycle;
mod stub {
    use super::CycleOutcome;
    use crate::admission::AdmittedTicket;
    use crate::config::roki::RokiConfig;
    use crate::config::workflow::Rule;
    use crate::error::PhaseInfraError;
    use std::path::Path;

    pub async fn run_cycle(
        _admitted: &AdmittedTicket,
        _rule: &Rule,
        _session_root: &Path,
        _cfg: &RokiConfig,
    ) -> Result<CycleOutcome, PhaseInfraError> {
        unimplemented!("run_cycle implemented in Task 11");
    }
}
