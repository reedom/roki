//! Per-state runner: render → spawn → wait → read sentinel → resolve edge.
//!
//! Spec: §2.4, §6 (data-flow capture).
//!
//! Slice 8 introduces this alongside the legacy `engine::phase` module.
//! Production wiring (real subprocess spawn) lands in Task 8; this file
//! defines the trait + types + a deterministic mock impl used by
//! `engine::cycle_state` integration tests.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::workflow::canonical::{EdgeTarget, State, StateId};

use super::outcome::FailureKind;
use super::sentinel::DirectivePayload;

/// Captured snapshot of a single state visit. Exposed downstream as
/// `{{ tasks.<state_id>.* }}` Liquid context and `ROKI_TASK_<ID>_*` env vars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCaptures {
    pub exit_code: i32,
    pub duration_seconds: u64,
    pub directive: Option<DirectivePayload>,
    /// Parsed claude/codex stream-json `result` event when applicable.
    pub terminal: Option<Value>,
}

impl TaskCaptures {
    /// Subset of fields safe to flatten into `ROKI_TASK_<ID>_*` env vars
    /// (top-level scalars only; complex objects stay Liquid-only).
    pub fn env_scalars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("EXIT_CODE", self.exit_code.to_string()),
            ("DURATION_SECONDS", self.duration_seconds.to_string()),
        ]
    }

    /// Top-level scalar fields from `directive.extra` safe for env exposure.
    /// Keys not matching `[A-Z0-9_]+` after uppercasing are skipped per
    /// spec §6.
    pub fn directive_extra_env(&self) -> Vec<(String, String)> {
        let Some(directive) = &self.directive else {
            return Vec::new();
        };
        directive
            .extra
            .iter()
            .filter_map(|(k, v)| {
                let upper = k.to_uppercase();
                if !upper.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return None;
                }
                let value_string = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => return None,
                };
                Some((upper, value_string))
            })
            .collect()
    }
}

/// Per-cycle context carried across state runs. Liquid render globals,
/// captured task outputs, env-var seed.
#[derive(Debug, Clone, Default)]
pub struct CycleContext {
    /// Stable identifiers (cycle.id, ticket.id, repo.ghq, cycle.kind, etc.).
    pub globals: Map<String, Value>,
    /// Per-state visit count (1-indexed during the visit).
    pub visits: BTreeMap<StateId, u32>,
    /// Capture history: most recent visit of each state.
    pub task_captures: BTreeMap<StateId, TaskCaptures>,
    /// Total state visits across this cycle.
    pub iter: u32,
    /// `roki.toml [engine].max_iterations`.
    pub max_iterations: u32,
}

impl CycleContext {
    pub fn record_capture(&mut self, state_id: &str, captures: TaskCaptures) {
        self.task_captures
            .insert(state_id.to_string(), captures);
    }

    pub fn bump_visit(&mut self, state_id: &str) -> u32 {
        let n = self.visits.entry(state_id.to_string()).or_insert(0);
        *n += 1;
        self.iter += 1;
        *n
    }
}

/// Outcome of running one state.
#[derive(Debug, Clone)]
pub enum StateOutcome {
    /// Normal exit: pick the next edge.
    Edge {
        next: EdgeTarget,
        captures: TaskCaptures,
    },
    /// Daemon-detected failure. Cycle aborts; routing to `on_failure` rules
    /// happens in `engine::cycle_state`.
    Failure {
        kind: FailureKind,
        error_text: String,
    },
}

/// One-state runner. Production impl spawns a subprocess; mock impls return
/// canned outcomes.
#[async_trait]
pub trait StateRunner: Send + Sync {
    async fn run_state(&self, state: &State, ctx: &CycleContext) -> StateOutcome;
}

/// Deterministic mock for cycle-driver integration tests. Maps
/// `(state_id, visit_n)` → canned outcome. Visit counter increments per call.
#[derive(Default, Clone)]
pub struct MockStateRunner {
    plans: Arc<Mutex<BTreeMap<(StateId, u32), StateOutcome>>>,
    default_outcome: Arc<Mutex<Option<StateOutcome>>>,
    call_log: Arc<Mutex<Vec<(StateId, u32)>>>,
}

impl MockStateRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plan(&self, state_id: &str, visit_n: u32, outcome: StateOutcome) -> &Self {
        self.plans
            .lock()
            .unwrap()
            .insert((state_id.to_string(), visit_n), outcome);
        self
    }

    /// Outcome to return when no specific plan matches. Defaults to a
    /// `__success__` edge with zero captures.
    pub fn set_default(&self, outcome: StateOutcome) -> &Self {
        *self.default_outcome.lock().unwrap() = Some(outcome);
        self
    }

    pub fn call_log(&self) -> Vec<(StateId, u32)> {
        self.call_log.lock().unwrap().clone()
    }
}

#[async_trait]
impl StateRunner for MockStateRunner {
    async fn run_state(&self, state: &State, ctx: &CycleContext) -> StateOutcome {
        let visit_n = ctx.visits.get(&state.id).copied().unwrap_or(1);
        self.call_log
            .lock()
            .unwrap()
            .push((state.id.clone(), visit_n));
        let plans = self.plans.lock().unwrap();
        if let Some(outcome) = plans.get(&(state.id.clone(), visit_n)) {
            return outcome.clone();
        }
        drop(plans);
        if let Some(default) = self.default_outcome.lock().unwrap().clone() {
            return default;
        }
        // Fallback: take on_done with empty captures.
        StateOutcome::Edge {
            next: state.on_done.clone(),
            captures: empty_captures(),
        }
    }
}

pub fn empty_captures() -> TaskCaptures {
    TaskCaptures {
        exit_code: 0,
        duration_seconds: 0,
        directive: None,
        terminal: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::canonical::test_helpers as h;

    #[tokio::test]
    async fn mock_returns_planned_outcome() {
        let runner = MockStateRunner::new();
        let outcome = StateOutcome::Edge {
            next: EdgeTarget::Terminal("__no_action__".into()),
            captures: empty_captures(),
        };
        runner.plan("judge", 1, outcome.clone());
        let state = h::state("judge", "echo");
        let mut ctx = CycleContext::default();
        ctx.bump_visit("judge");
        let got = runner.run_state(&state, &ctx).await;
        assert!(matches!(
            got,
            StateOutcome::Edge { next: EdgeTarget::Terminal(ref id), .. }
            if id == "__no_action__"
        ));
        assert_eq!(runner.call_log(), vec![("judge".to_string(), 1)]);
    }

    #[tokio::test]
    async fn mock_falls_back_to_on_done_when_no_plan() {
        let runner = MockStateRunner::new();
        let mut state = h::state("impl", "echo");
        state.on_done = EdgeTarget::Terminal("__success__".into());
        let mut ctx = CycleContext::default();
        ctx.bump_visit("impl");
        let got = runner.run_state(&state, &ctx).await;
        match got {
            StateOutcome::Edge {
                next: EdgeTarget::Terminal(ref id),
                ..
            } => assert_eq!(id, "__success__"),
            other => panic!("expected Edge to terminal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_default_overrides_on_done_fallback() {
        let runner = MockStateRunner::new();
        runner.set_default(StateOutcome::Failure {
            kind: FailureKind::ProcessCrash,
            error_text: "synthetic".into(),
        });
        let state = h::state("impl", "echo");
        let mut ctx = CycleContext::default();
        ctx.bump_visit("impl");
        let got = runner.run_state(&state, &ctx).await;
        assert!(matches!(got, StateOutcome::Failure { kind: FailureKind::ProcessCrash, .. }));
    }

    #[test]
    fn task_captures_env_scalars_emit_expected_keys() {
        let c = TaskCaptures {
            exit_code: 0,
            duration_seconds: 5,
            directive: None,
            terminal: None,
        };
        let envs: Vec<_> = c.env_scalars().into_iter().collect();
        assert!(envs.iter().any(|(k, _)| *k == "EXIT_CODE"));
        assert!(envs.iter().any(|(k, _)| *k == "DURATION_SECONDS"));
    }

    #[test]
    fn directive_extra_env_filters_non_env_safe_keys() {
        use serde_json::json;
        let mut extra = Map::new();
        extra.insert("verdict".into(), json!("ok"));
        extra.insert("space key".into(), json!("dropped")); // contains space → skip
        extra.insert("nested".into(), json!({ "x": 1 })); // object → skip
        let c = TaskCaptures {
            exit_code: 0,
            duration_seconds: 0,
            directive: Some(DirectivePayload {
                directive: "end".into(),
                outcome: None,
                extra,
            }),
            terminal: None,
        };
        let envs: Vec<_> = c.directive_extra_env();
        let keys: Vec<&str> = envs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"VERDICT"));
        assert!(!keys.iter().any(|k| k.contains(' ')));
        assert!(!keys.contains(&"NESTED"));
    }

    #[test]
    fn cycle_context_bump_visit_increments_counters() {
        let mut ctx = CycleContext::default();
        assert_eq!(ctx.bump_visit("a"), 1);
        assert_eq!(ctx.bump_visit("a"), 2);
        assert_eq!(ctx.bump_visit("b"), 1);
        assert_eq!(ctx.iter, 3);
    }
}
