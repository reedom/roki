//! Validation for canonical `WorkflowFile`.
//!
//! Spec: §4.4 (Pass 4 — validation).
//!
//! Multi-error: every rule's state machine is checked end-to-end and every
//! violation is collected before returning. Errors sort deterministically.

#![allow(dead_code)]

use thiserror::Error;

use super::canonical::{
    EdgeTarget, State, StateBody, StateId, StateMachine, Terminal, WorkflowFile,
};
use super::sugar;

#[derive(Debug, Error, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum ValidationError {
    #[error("rule[{rule_idx}] state '{state_id}' edge target '{target}' is undeclared")]
    UnknownEdgeTarget {
        rule_idx: usize,
        state_id: StateId,
        target: StateId,
    },
    #[error("rule[{rule_idx}] state '{state_id}' has empty body (no run: and no uses:)")]
    OrphanBody { rule_idx: usize, state_id: StateId },
    #[error("rule[{rule_idx}] state '{state_id}' uses reserved __* prefix")]
    ReservedPrefixState { rule_idx: usize, state_id: StateId },
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
    InvalidStartReference { rule_idx: usize, start: StateId },
    #[error(
        "rule[{rule_idx}] state id '{state_id}' is not env-var-safe (must match [A-Za-z][A-Za-z0-9_]*)"
    )]
    StateIdNotEnvSafe { rule_idx: usize, state_id: StateId },
}

/// Validate a fully-expanded `WorkflowFile`. Accumulates all errors before
/// returning. Errors are sorted deterministically.
///
/// Rule numbering follows spec §4.4.
pub fn run(file: &WorkflowFile) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let mut absolute_idx: usize = 0;
    for section in [&file.rules, &file.cleanup, &file.on_failure] {
        for rule in section {
            validate_rule(&rule.state_machine, absolute_idx, &mut errors);
            absolute_idx += 1;
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        errors.sort();
        Err(errors)
    }
}

fn validate_rule(sm: &StateMachine, rule_idx: usize, errors: &mut Vec<ValidationError>) {
    validate_start(sm, rule_idx, errors);
    for (id, state) in &sm.states {
        validate_state_id(id, rule_idx, errors);
        validate_state_body(state, rule_idx, errors);
        validate_state_edges(sm, state, rule_idx, errors);
    }
    validate_terminals(sm, rule_idx, errors);
    validate_cycles_bounded(sm, rule_idx, errors);
}

fn validate_start(sm: &StateMachine, rule_idx: usize, errors: &mut Vec<ValidationError>) {
    // Cleanup immediate-delete shorthand legitimately starts at a terminal id;
    // exempt that case (no states, start id ∈ terminals).
    if sm.states.is_empty() && sm.terminals.contains_key(&sm.start) {
        return;
    }
    if !sm.states.contains_key(&sm.start) || sm.terminals.contains_key(&sm.start) {
        errors.push(ValidationError::InvalidStartReference {
            rule_idx,
            start: sm.start.clone(),
        });
    }
}

fn validate_state_id(id: &StateId, rule_idx: usize, errors: &mut Vec<ValidationError>) {
    if id.starts_with("__") {
        errors.push(ValidationError::ReservedPrefixState {
            rule_idx,
            state_id: id.clone(),
        });
        return;
    }
    if !is_env_safe_id(id) {
        errors.push(ValidationError::StateIdNotEnvSafe {
            rule_idx,
            state_id: id.clone(),
        });
    }
}

fn is_env_safe_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate_state_body(state: &State, rule_idx: usize, errors: &mut Vec<ValidationError>) {
    let orphan = match &state.body {
        StateBody::Run { cmd } => cmd.trim().is_empty(),
        StateBody::Uses { path } => path.as_os_str().is_empty(),
    };
    if orphan {
        errors.push(ValidationError::OrphanBody {
            rule_idx,
            state_id: state.id.clone(),
        });
    }
}

fn validate_state_edges(
    sm: &StateMachine,
    state: &State,
    rule_idx: usize,
    errors: &mut Vec<ValidationError>,
) {
    check_target(&state.on_done, sm, rule_idx, &state.id, errors);
    check_target(&state.on_fail, sm, rule_idx, &state.id, errors);
    for target in state.directives.values() {
        check_target(target, sm, rule_idx, &state.id, errors);
    }
}

fn check_target(
    t: &EdgeTarget,
    sm: &StateMachine,
    rule_idx: usize,
    state_id: &str,
    errors: &mut Vec<ValidationError>,
) {
    let id = match t {
        EdgeTarget::State(s) | EdgeTarget::Terminal(s) => s,
    };
    if !sm.states.contains_key(id) && !sm.terminals.contains_key(id) {
        errors.push(ValidationError::UnknownEdgeTarget {
            rule_idx,
            state_id: state_id.to_string(),
            target: id.clone(),
        });
    }
}

fn validate_terminals(sm: &StateMachine, rule_idx: usize, errors: &mut Vec<ValidationError>) {
    for (id, terminal) in &sm.terminals {
        validate_terminal(id, terminal, rule_idx, errors);
    }
}

fn validate_terminal(
    id: &StateId,
    terminal: &Terminal,
    rule_idx: usize,
    errors: &mut Vec<ValidationError>,
) {
    if terminal.outcome.trim().is_empty() {
        errors.push(ValidationError::EmptyTerminalOutcome {
            rule_idx,
            terminal_id: id.clone(),
        });
    }
}

/// Walk the SCCs computed in sugar's Tarjan helper. For each SCC that
/// represents a cycle (≥2 nodes, OR 1 node with a self-edge), verify at
/// least one member has `max_visits > 1`. Pass 5 should already have
/// auto-injected the default cap, so this surfaces only genuine bugs in
/// the expansion pipeline or ill-formed inputs that bypass it.
fn validate_cycles_bounded(sm: &StateMachine, rule_idx: usize, errors: &mut Vec<ValidationError>) {
    let sccs = sugar::tarjan_scc(sm);
    for scc in sccs {
        let on_cycle =
            scc.len() >= 2 || (scc.len() == 1 && sugar::state_has_self_edge(sm, &scc[0]));
        if !on_cycle {
            continue;
        }
        let any_bound = scc
            .iter()
            .any(|id| sm.states.get(id).map(|s| s.max_visits > 1).unwrap_or(false));
        if !any_bound {
            let mut sorted = scc.clone();
            sorted.sort();
            errors.push(ValidationError::UnboundedCycle {
                rule_idx,
                state_ids: sorted,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::canonical::test_helpers as h;
    use crate::workflow::canonical::{EdgeTarget, RuleEntry, StateBody};

    fn one_rule(sm: StateMachine) -> WorkflowFile {
        WorkflowFile {
            admission: None,
            rules: vec![RuleEntry {
                when: None,
                state_machine: sm,
            }],
            cleanup: vec![],
            on_failure: vec![],
        }
    }

    #[test]
    fn ok_minimal_rule() {
        let mut sm = h::state_machine();
        sm.start = "impl".into();
        sm.states.insert("impl".into(), h::state("impl", "echo"));
        sm.terminals
            .insert("__success__".into(), h::terminal("__success__", "success"));
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        run(&one_rule(sm)).unwrap();
    }

    #[test]
    fn unknown_edge_target_flagged() {
        let mut sm = h::state_machine();
        sm.start = "a".into();
        let mut a = h::state("a", "x");
        a.on_done = EdgeTarget::State("missing".into());
        sm.states.insert("a".into(), a);
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(matches!(
            &errs[0],
            ValidationError::UnknownEdgeTarget { target, .. } if target == "missing"
        ));
    }

    #[test]
    fn reserved_prefix_flagged() {
        let mut sm = h::state_machine();
        sm.start = "__internal".into();
        sm.states
            .insert("__internal".into(), h::state("__internal", "x"));
        sm.terminals
            .insert("__success__".into(), h::terminal("__success__", "success"));
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::ReservedPrefixState { .. }))
        );
    }

    #[test]
    fn invalid_start_flagged() {
        let mut sm = h::state_machine();
        sm.start = "missing".into();
        sm.states.insert("a".into(), h::state("a", "x"));
        sm.terminals
            .insert("__success__".into(), h::terminal("__success__", "success"));
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::InvalidStartReference { .. }))
        );
    }

    #[test]
    fn empty_terminal_outcome_flagged() {
        let mut sm = h::state_machine();
        sm.start = "a".into();
        sm.states.insert("a".into(), h::state("a", "x"));
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "".into(),
            },
        );
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::EmptyTerminalOutcome { .. }))
        );
    }

    #[test]
    fn orphan_body_flagged_when_run_empty() {
        let mut sm = h::state_machine();
        sm.start = "a".into();
        let mut a = h::state("a", "");
        a.body = StateBody::Run { cmd: "".into() };
        sm.states.insert("a".into(), a);
        sm.terminals
            .insert("__success__".into(), h::terminal("__success__", "success"));
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::OrphanBody { .. }))
        );
    }

    #[test]
    fn unbounded_cycle_flagged() {
        let mut sm = h::state_machine();
        sm.start = "a".into();
        // a → a self-loop, no max_visits > 1
        let mut a = h::state("a", "x");
        a.on_done = EdgeTarget::State("a".into()); // self-loop
        sm.states.insert("a".into(), a);
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::UnboundedCycle { .. }))
        );
    }

    #[test]
    fn bounded_self_loop_passes() {
        let mut sm = h::state_machine();
        sm.start = "a".into();
        let mut a = h::state("a", "x");
        a.on_done = EdgeTarget::State("a".into());
        a.max_visits = 5;
        sm.states.insert("a".into(), a);
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        run(&one_rule(sm)).unwrap();
    }

    #[test]
    fn state_id_not_env_safe_flagged() {
        let mut sm = h::state_machine();
        sm.start = "9_starts_with_digit".into();
        sm.states.insert(
            "9_starts_with_digit".into(),
            h::state("9_starts_with_digit", "x"),
        );
        sm.terminals
            .insert("__success__".into(), h::terminal("__success__", "success"));
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::StateIdNotEnvSafe { .. }))
        );
    }

    #[test]
    fn state_id_dashes_flagged() {
        let mut sm = h::state_machine();
        sm.start = "with-dash".into();
        sm.states
            .insert("with-dash".into(), h::state("with-dash", "x"));
        sm.terminals
            .insert("__success__".into(), h::terminal("__success__", "success"));
        sm.terminals
            .insert("__failure__".into(), h::terminal("__failure__", "failure"));
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::StateIdNotEnvSafe { .. }))
        );
    }

    #[test]
    fn multi_error_accumulates() {
        let mut sm = h::state_machine();
        sm.start = "missing".into(); // InvalidStartReference
        let mut a = h::state("__bad", ""); // ReservedPrefix + OrphanBody
        a.on_done = EdgeTarget::State("nowhere".into()); // UnknownEdgeTarget
        sm.states.insert("__bad".into(), a);
        sm.terminals.insert(
            "empty".into(),
            Terminal {
                id: "empty".into(),
                outcome: "".into(),
            },
        );
        let errs = run(&one_rule(sm)).unwrap_err();
        assert!(errs.len() >= 4, "got {errs:?}");
    }

    #[test]
    fn cleanup_immediate_delete_passes() {
        let mut sm = h::state_machine();
        sm.start = "__success__".into();
        sm.terminals
            .insert("__success__".into(), h::terminal("__success__", "cleaned"));
        let mut file = WorkflowFile {
            admission: None,
            rules: vec![],
            cleanup: vec![RuleEntry {
                when: None,
                state_machine: sm,
            }],
            on_failure: vec![],
        };
        run(&file).unwrap();
        // Avoid unused-mut warning in case I expand the test later.
        file.rules.clear();
    }
}
