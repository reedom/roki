//! Validation for canonical `WorkflowFile`. Filled in by Task 4.
//!
//! Spec: §4.4 (Pass 4 — validation).

#![allow(dead_code)]

use thiserror::Error;

use super::canonical::{StateId, WorkflowFile};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("rule[{rule_idx}] state '{state_id}' edge target '{target}' is undeclared")]
    UnknownEdgeTarget {
        rule_idx: usize,
        state_id: StateId,
        target: StateId,
    },
    #[error("rule[{rule_idx}] state '{state_id}' declared twice")]
    DuplicateStateId {
        rule_idx: usize,
        state_id: StateId,
    },
    #[error("rule[{rule_idx}] state '{state_id}' has both run: and uses:")]
    BothRunAndUses {
        rule_idx: usize,
        state_id: StateId,
    },
    #[error("rule[{rule_idx}] state '{state_id}' has neither run: nor uses:")]
    OrphanBody {
        rule_idx: usize,
        state_id: StateId,
    },
    #[error("rule[{rule_idx}] state '{state_id}' uses reserved __* prefix")]
    ReservedPrefixState {
        rule_idx: usize,
        state_id: StateId,
    },
    #[error("rule[{rule_idx}] cycle through {state_ids:?} has no max_visits")]
    UnboundedCycle {
        rule_idx: usize,
        state_ids: Vec<StateId>,
    },
    #[error("rule[{rule_idx}] terminal '{terminal_id}' has empty outcome")]
    EmptyTerminalOutcome {
        rule_idx: usize,
        terminal_id: StateId,
    },
    #[error("rule[{rule_idx}] start references invalid state '{start}'")]
    InvalidStartReference {
        rule_idx: usize,
        start: StateId,
    },
    #[error("rule[{rule_idx}] state id '{state_id}' is not env-var-safe (must match [A-Za-z][A-Za-z0-9_]*)")]
    StateIdNotEnvSafe {
        rule_idx: usize,
        state_id: StateId,
    },
}

/// Stub: real implementation lands in Task 4. Always returns Ok so Task 3
/// (sugar expansion) can wire Pass 4 in advance.
pub fn run(_file: &WorkflowFile) -> Result<(), Vec<ValidationError>> {
    Ok(())
}
