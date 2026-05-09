//! Canonical workflow types.
//!
//! Spec: `docs/superpowers/specs/2026-05-09-slice8-workflow-yaml-statemachine-design.md`
//! §2.2 (Types). Post-sugar-expansion shape consumed by the cycle engine.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub type StateId = String;
pub type DirectiveName = String;

/// Unparsed Liquid template body. Rendered via `engine::template` at use time.
pub type LiquidString = String;

/// Unparsed Liquid expression for a state's `if:` condition.
pub type LiquidExpr = String;

#[derive(Debug, Clone)]
pub struct WorkflowFile {
    /// `None` in per-repo override files (admission lives only in the top-level file).
    pub admission: Option<Admission>,
    pub rules: Vec<RuleEntry>,
    pub cleanup: Vec<RuleEntry>,
    pub on_failure: Vec<RuleEntry>,
}

#[derive(Debug, Clone)]
pub struct Admission {
    /// Literal `"me"` is reserved and resolved to the API token holder by the admission resolver.
    pub assignee: String,
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone)]
pub struct RepoEntry {
    pub ghq: String,
    pub when: Option<WhenClause>,
    /// Path to a per-repo override workflow file. Resolution rules: spec §3.2.1.
    pub workflow: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RuleEntry {
    pub when: Option<WhenClause>,
    pub state_machine: StateMachine,
}

#[derive(Debug, Clone, Default)]
pub struct WhenClause {
    pub status: Option<ScalarMatcher>,
    pub labels: Option<LabelsMatcher>,
    pub assignee: Option<ScalarMatcher>,
    pub repo: Option<String>,
    pub kind: Option<ScalarMatcher>,
    pub phase: Option<ScalarMatcher>,
    pub title: Option<TextMatcher>,
    pub body: Option<TextMatcher>,
}

#[derive(Debug, Clone)]
pub enum ScalarMatcher {
    Eq(String),
    Not(String),
    In(Vec<String>),
}

#[derive(Debug, Clone, Default)]
pub struct LabelsMatcher {
    pub has_all: Option<Vec<String>>,
    pub has_any: Option<Vec<String>>,
    pub has_none: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub enum TextMatcher {
    Regex(String),
    StartsWith(String),
    Contains(String),
}

#[derive(Debug, Clone)]
pub struct StateMachine {
    pub start: StateId,
    pub states: BTreeMap<StateId, State>,
    pub terminals: BTreeMap<StateId, Terminal>,
}

#[derive(Debug, Clone)]
pub struct State {
    pub id: StateId,
    pub body: StateBody,
    pub if_cond: Option<LiquidExpr>,
    pub on_done: EdgeTarget,
    pub on_fail: EdgeTarget,
    pub directives: BTreeMap<DirectiveName, EdgeTarget>,
    pub max_visits: u32,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub enum StateBody {
    Run { cmd: LiquidString },
    Uses { path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Terminal {
    pub id: StateId,
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeTarget {
    State(StateId),
    Terminal(StateId),
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;

    pub fn state_machine() -> StateMachine {
        StateMachine {
            start: "start".to_string(),
            states: BTreeMap::new(),
            terminals: BTreeMap::new(),
        }
    }

    pub fn state(id: &str, cmd: &str) -> State {
        State {
            id: id.to_string(),
            body: StateBody::Run {
                cmd: cmd.to_string(),
            },
            if_cond: None,
            on_done: EdgeTarget::Terminal("__success__".to_string()),
            on_fail: EdgeTarget::Terminal("__failure__".to_string()),
            directives: BTreeMap::new(),
            max_visits: 1,
            timeout: None,
        }
    }

    pub fn terminal(id: &str, outcome: &str) -> Terminal {
        Terminal {
            id: id.to_string(),
            outcome: outcome.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_helper_starts_empty() {
        let sm = test_helpers::state_machine();
        assert_eq!(sm.start, "start");
        assert!(sm.states.is_empty());
        assert!(sm.terminals.is_empty());
    }

    #[test]
    fn state_helper_defaults_to_implicit_terminals() {
        let s = test_helpers::state("impl", "true");
        assert_eq!(s.id, "impl");
        match &s.body {
            StateBody::Run { cmd } => assert_eq!(cmd, "true"),
            StateBody::Uses { .. } => panic!("expected Run body"),
        }
        assert_eq!(s.on_done, EdgeTarget::Terminal("__success__".into()));
        assert_eq!(s.on_fail, EdgeTarget::Terminal("__failure__".into()));
        assert_eq!(s.max_visits, 1);
        assert!(s.directives.is_empty());
    }

    #[test]
    fn terminal_helper_round_trips_outcome() {
        let t = test_helpers::terminal("__success__", "success");
        assert_eq!(t.id, "__success__");
        assert_eq!(t.outcome, "success");
    }

    #[test]
    fn states_iteration_is_deterministic() {
        let mut sm = test_helpers::state_machine();
        sm.states.insert("c".into(), test_helpers::state("c", "x"));
        sm.states.insert("a".into(), test_helpers::state("a", "x"));
        sm.states.insert("b".into(), test_helpers::state("b", "x"));
        let ids: Vec<_> = sm.states.keys().cloned().collect();
        assert_eq!(ids, vec!["a".to_string(), "b".into(), "c".into()]);
    }

    #[test]
    fn edge_target_equality() {
        assert_eq!(
            EdgeTarget::State("foo".into()),
            EdgeTarget::State("foo".into())
        );
        assert_ne!(
            EdgeTarget::State("foo".into()),
            EdgeTarget::Terminal("foo".into())
        );
    }
}
