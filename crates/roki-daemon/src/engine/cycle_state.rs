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

        // Liquid `if:` skip — when the expression evaluates falsy the state
        // does not spawn a subprocess; the cycle advances to `on_done` with
        // empty captures (spec §2.4 cycle runtime loop). Render error → the
        // skip evaluation reports `template_error` like a body render error.
        if let Some(expr) = &state.if_cond {
            let liquid_globals = build_skip_globals(state, ctx, visit_n);
            match crate::engine::template::eval_cond(expr, &liquid_globals) {
                Ok(true) => { /* fall through and run the state */ }
                Ok(false) => {
                    current_id = resolve_target(state.on_done.clone());
                    continue;
                }
                Err(err) => {
                    return Err(FailureMetadata {
                        kind: FailureKind::TemplateError,
                        state_id: current_id.clone(),
                        visit_n,
                        error_text: format!("if_cond render: {err}"),
                    });
                }
            }
        }

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

/// Build the Liquid globals object used to evaluate `state.if_cond`. Mirrors
/// the per-state shape produced by `engine::real_state_runner` so operators
/// see the same data in both surfaces. Kept minimal here — the production
/// runner builds the full object including past task captures and ROKI_*
/// scalars; the skip evaluator only needs `cycle.iter`, `state.id`,
/// `state.visit_n`, and any `tasks.*` already accumulated.
fn build_skip_globals(
    state: &crate::workflow::canonical::State,
    ctx: &CycleContext,
    visit_n: u32,
) -> liquid::Object {
    use liquid::model::Value;

    let mut globals: liquid::Object = ctx
        .globals
        .iter()
        .map(|(k, v)| {
            (
                k.clone().into(),
                liquid::model::to_value(v).unwrap_or(Value::Nil),
            )
        })
        .collect();

    if let Some(Value::Object(cycle)) = globals.get_mut("cycle") {
        cycle.insert("iter".into(), Value::scalar(ctx.iter as i64));
    }

    let mut state_obj = liquid::Object::new();
    state_obj.insert("id".into(), Value::scalar(state.id.clone()));
    state_obj.insert("visit_n".into(), Value::scalar(visit_n as i64));
    globals.insert("state".into(), Value::Object(state_obj));

    let mut tasks_obj = liquid::Object::new();
    for (id, captures) in &ctx.task_captures {
        let mut entry = liquid::Object::new();
        entry.insert("exit_code".into(), Value::scalar(captures.exit_code as i64));
        entry.insert(
            "duration_seconds".into(),
            Value::scalar(captures.duration_seconds as i64),
        );
        if let Some(d) = &captures.directive {
            let mut dobj = liquid::Object::new();
            dobj.insert("directive".into(), Value::scalar(d.directive.clone()));
            if let Some(o) = &d.outcome {
                dobj.insert("outcome".into(), Value::scalar(o.clone()));
            }
            for (k, v) in &d.extra {
                dobj.insert(
                    k.clone().into(),
                    liquid::model::to_value(v).unwrap_or(Value::Nil),
                );
            }
            entry.insert("directive".into(), Value::Object(dobj));
        }
        tasks_obj.insert(id.clone().into(), Value::Object(entry));
    }
    globals.insert("tasks".into(), Value::Object(tasks_obj));

    globals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::state_runtime::{MockStateRunner, TaskCaptures, empty_captures};
    use crate::workflow::canonical::Terminal;
    use crate::workflow::canonical::test_helpers as h;

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
        use crate::engine::sentinel::DirectivePayload;
        use serde_json::Map;

        let mut sm = h::state_machine();
        sm.start = "a".into();
        let mut a = h::state("a", "x");
        a.directives
            .insert("end".into(), EdgeTarget::Terminal("__success__".into()));
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
    async fn if_cond_falsy_skips_state_to_on_done() {
        let mut sm = h::state_machine();
        sm.start = "guard".into();
        let mut guard = h::state("guard", "x");
        guard.if_cond = Some("ghost".into()); // missing var → falsy
        guard.on_done = EdgeTarget::Terminal("__success__".into());
        sm.states.insert("guard".into(), guard);
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "skipped".into(),
            },
        );

        let runner = MockStateRunner::new();
        let mut ctx = CycleContext::default();
        let result = run_cycle(&sm, &runner, &mut ctx).await.unwrap();
        assert_eq!(result.terminal_id, "__success__");
        // Skip means the runner is never called.
        assert!(runner.call_log().is_empty());
        // visit_n still increments because the state was entered.
        assert_eq!(ctx.iter, 1);
    }

    #[tokio::test]
    async fn if_cond_truthy_runs_state() {
        let mut sm = h::state_machine();
        sm.start = "guard".into();
        let mut guard = h::state("guard", "x");
        guard.if_cond = Some("flag".into());
        guard.on_done = EdgeTarget::Terminal("__success__".into());
        sm.states.insert("guard".into(), guard);
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "ran".into(),
            },
        );

        let runner = MockStateRunner::new();
        let mut ctx = CycleContext::default();
        ctx.globals
            .insert("flag".into(), serde_json::Value::String("yes".into()));
        let result = run_cycle(&sm, &runner, &mut ctx).await.unwrap();
        assert_eq!(result.terminal_id, "__success__");
        assert_eq!(runner.call_log(), vec![("guard".into(), 1)]);
    }

    #[tokio::test]
    async fn if_cond_render_error_returns_template_error() {
        let mut sm = h::state_machine();
        sm.start = "guard".into();
        let mut guard = h::state("guard", "x");
        // Unmatched braces → Liquid parser error inside the wrapper.
        guard.if_cond = Some("flag and {%".into());
        sm.states.insert("guard".into(), guard);
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
        assert_eq!(err.kind, FailureKind::TemplateError);
        assert_eq!(err.state_id, "guard");
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
