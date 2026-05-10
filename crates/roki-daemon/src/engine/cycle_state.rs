//! State-machine cycle driver.
//!
//! Spec: §2.4 (cycle runtime loop).
//!
//! Replaces the legacy `engine::cycle` pre/run/post phase loop. Drives a
//! `StateMachine` to completion: visit a state, run it via a `StateRunner`,
//! resolve the next edge, repeat until landing in a terminal or hitting a
//! daemon-detected failure.

#![allow(dead_code)]

use crate::workflow::canonical::{EdgeTarget, StateId, StateMachine};

use super::outcome::FailureKind;
use super::state_runtime::{CycleContext, StateOutcome, StateRunner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleResult {
    pub terminal_id: StateId,
    pub outcome: String,
    /// Total state visits across this cycle.
    pub iterations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureMetadata {
    pub kind: FailureKind,
    pub state_id: StateId,
    pub visit_n: u32,
    pub error_text: String,
}

/// Drive a state machine to a terminal or to a daemon-detected failure.
///
/// `ctx` carries the per-cycle Liquid globals + accumulated task captures.
/// On entry, `ctx.iter` should be 0 and `ctx.visits` empty; the driver
/// mutates them as it runs.
///
/// Outcome override: when a sentinel-driven directive points to a terminal
/// and carries an `outcome` field, that string overrides the terminal's
/// declared outcome for this cycle's `CycleResult`.
pub async fn run_cycle<R>(
    sm: &StateMachine,
    runner: &R,
    ctx: &mut CycleContext,
) -> Result<CycleResult, FailureMetadata>
where
    R: StateRunner + ?Sized,
{
    let mut current_id: StateId = sm.start.clone();
    let mut terminal_outcome_override: Option<String> = None;

    loop {
        if let Some(terminal) = sm.terminals.get(&current_id) {
            return Ok(CycleResult {
                terminal_id: terminal.id.clone(),
                outcome: terminal_outcome_override.unwrap_or_else(|| terminal.outcome.clone()),
                iterations: ctx.iter,
            });
        }

        let Some(state) = sm.states.get(&current_id) else {
            return Err(FailureMetadata {
                kind: FailureKind::SchemaDrift,
                state_id: current_id.clone(),
                visit_n: ctx.visits.get(&current_id).copied().unwrap_or(0),
                error_text: format!("state '{current_id}' not found in machine"),
            });
        };

        let visit_n = ctx.bump_visit(&current_id);
        if visit_n > state.max_visits {
            return Err(FailureMetadata {
                kind: FailureKind::RecursionBound,
                state_id: current_id.clone(),
                visit_n,
                error_text: format!(
                    "state '{current_id}' visited {visit_n} times; max_visits={}",
                    state.max_visits
                ),
            });
        }

        // Liquid `if:` skip lands in Task 8 alongside the real subprocess
        // runner. For now, treat `if_cond.is_some()` as always-true.

        let outcome = runner.run_state(state, ctx).await;
        match outcome {
            StateOutcome::Edge { next, captures } => {
                ctx.record_capture(&current_id, captures.clone());
                if let Some(directive) = &captures.directive {
                    if let Some(o) = &directive.outcome {
                        terminal_outcome_override = Some(o.clone());
                    }
                }
                current_id = resolve_target(next);
            }
            StateOutcome::Failure { kind, error_text } => {
                return Err(FailureMetadata {
                    kind,
                    state_id: current_id,
                    visit_n,
                    error_text,
                });
            }
        }
    }
}

fn resolve_target(target: EdgeTarget) -> StateId {
    match target {
        EdgeTarget::State(id) | EdgeTarget::Terminal(id) => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::state_runtime::{empty_captures, MockStateRunner, TaskCaptures};
    use crate::workflow::canonical::test_helpers as h;
    use crate::workflow::canonical::{State, StateBody, Terminal};
    use std::collections::BTreeMap;

    fn linear_chain(ids: &[&str]) -> StateMachine {
        let mut sm = h::state_machine();
        sm.start = ids[0].to_string();
        for (i, id) in ids.iter().enumerate() {
            let mut state = h::state(id, "x");
            state.on_done = if i + 1 < ids.len() {
                EdgeTarget::State(ids[i + 1].to_string())
            } else {
                EdgeTarget::Terminal("__success__".to_string())
            };
            sm.states.insert(id.to_string(), state);
        }
        sm.terminals.insert(
            "__success__".to_string(),
            Terminal {
                id: "__success__".to_string(),
                outcome: "success".to_string(),
            },
        );
        sm.terminals.insert(
            "__failure__".to_string(),
            Terminal {
                id: "__failure__".to_string(),
                outcome: "failure".to_string(),
            },
        );
        sm
    }

    #[tokio::test]
    async fn linear_chain_walks_to_success() {
        let sm = linear_chain(&["judge", "impl", "verdict"]);
        let runner = MockStateRunner::new();
        let mut ctx = CycleContext::default();
        let result = run_cycle(&sm, &runner, &mut ctx).await.unwrap();
        assert_eq!(result.terminal_id, "__success__");
        assert_eq!(result.outcome, "success");
        assert_eq!(result.iterations, 3);
        assert_eq!(
            runner.call_log(),
            vec![
                ("judge".into(), 1),
                ("impl".into(), 1),
                ("verdict".into(), 1),
            ]
        );
    }

    #[tokio::test]
    async fn self_loop_with_max_visits_yields_recursion_bound() {
        let mut sm = h::state_machine();
        sm.start = "a".into();
        let mut a = h::state("a", "x");
        a.on_done = EdgeTarget::State("a".into()); // self-loop
        a.max_visits = 2;
        sm.states.insert("a".into(), a);
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "success".into(),
            },
        );

        let runner = MockStateRunner::new();
        let mut ctx = CycleContext::default();
        let err = run_cycle(&sm, &runner, &mut ctx).await.unwrap_err();
        assert_eq!(err.kind, FailureKind::RecursionBound);
        assert_eq!(err.state_id, "a");
        assert_eq!(err.visit_n, 3);
    }

    #[tokio::test]
    async fn directive_outcome_field_overrides_terminal_outcome() {
        use serde_json::Map;
        use crate::engine::sentinel::DirectivePayload;

        let mut sm = h::state_machine();
        sm.start = "a".into();
        let mut a = h::state("a", "x");
        a.directives.insert(
            "end".into(),
            EdgeTarget::Terminal("__success__".into()),
        );
        sm.states.insert("a".into(), a);
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "success".into(),
            },
        );

        let runner = MockStateRunner::new();
        runner.plan(
            "a",
            1,
            StateOutcome::Edge {
                next: EdgeTarget::Terminal("__success__".into()),
                captures: TaskCaptures {
                    exit_code: 0,
                    duration_seconds: 0,
                    directive: Some(DirectivePayload {
                        directive: "end".into(),
                        outcome: Some("custom_outcome".into()),
                        extra: Map::new(),
                    }),
                    terminal: None,
                },
            },
        );
        let mut ctx = CycleContext::default();
        let result = run_cycle(&sm, &runner, &mut ctx).await.unwrap();
        assert_eq!(result.outcome, "custom_outcome");
    }

    #[tokio::test]
    async fn failure_outcome_returns_metadata_with_state_id_and_visit_n() {
        let sm = linear_chain(&["a", "b"]);
        let runner = MockStateRunner::new();
        runner.plan(
            "a",
            1,
            StateOutcome::Failure {
                kind: FailureKind::ProcessCrash,
                error_text: "killed".into(),
            },
        );
        let mut ctx = CycleContext::default();
        let err = run_cycle(&sm, &runner, &mut ctx).await.unwrap_err();
        assert_eq!(err.kind, FailureKind::ProcessCrash);
        assert_eq!(err.state_id, "a");
        assert_eq!(err.visit_n, 1);
        assert_eq!(err.error_text, "killed");
    }

    #[tokio::test]
    async fn cleanup_immediate_delete_starts_at_terminal() {
        let mut sm = h::state_machine();
        sm.start = "__success__".into();
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "cleaned".into(),
            },
        );
        let runner = MockStateRunner::new();
        let mut ctx = CycleContext::default();
        let result = run_cycle(&sm, &runner, &mut ctx).await.unwrap();
        assert_eq!(result.terminal_id, "__success__");
        assert_eq!(result.outcome, "cleaned");
        assert_eq!(result.iterations, 0);
        assert!(runner.call_log().is_empty());
    }

    #[tokio::test]
    async fn directive_to_state_traversal() {
        // a -> b via directive "skip"; b -> __success__
        let mut sm = h::state_machine();
        sm.start = "a".into();
        let mut a = h::state("a", "x");
        a.directives
            .insert("skip".into(), EdgeTarget::State("b".into()));
        sm.states.insert("a".into(), a);
        let mut b = h::state("b", "y");
        b.on_done = EdgeTarget::Terminal("__success__".into());
        sm.states.insert("b".into(), b);
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "success".into(),
            },
        );

        let runner = MockStateRunner::new();
        runner.plan(
            "a",
            1,
            StateOutcome::Edge {
                next: EdgeTarget::State("b".into()),
                captures: empty_captures(),
            },
        );
        let mut ctx = CycleContext::default();
        let result = run_cycle(&sm, &runner, &mut ctx).await.unwrap();
        assert_eq!(result.terminal_id, "__success__");
        assert_eq!(runner.call_log(), vec![("a".into(), 1), ("b".into(), 1)]);
    }
}
