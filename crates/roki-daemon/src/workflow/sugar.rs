//! Sugar → canonical 5-pass expansion.
//!
//! Spec: §4.1 - §4.5.
//!
//! - Pass 1: implicit terminals injection (`__success__`, `__failure__`,
//!   `__no_action__`, `__cancelled__`).
//! - Pass 2: `tasks:` array → states + default-chained edges.
//! - Pass 3: directive name defaults — runtime-only via `canonical::resolve_directive`.
//! - Pass 4: validation via `workflow::validate::run`.
//! - Pass 5: auto-`max_visits` injection on SCC entry nodes (Tarjan).

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use thiserror::Error;

use super::canonical::{
    Admission, DirectiveName, EdgeTarget, LabelsMatcher, RepoEntry, RuleEntry, ScalarMatcher,
    State, StateBody, StateId, StateMachine, Terminal, TextMatcher, WhenClause, WorkflowFile,
};
use super::parse::{
    RawAdmission, RawDirectiveTarget, RawLabelsMatcher, RawRepoEntry, RawRuleBody, RawRuleEntry,
    RawScalarMatcher, RawStateEntry, RawTaskEntry, RawTerminalEntry, RawTextMatcher, RawWhenClause,
    RawWorkflow,
};
use super::validate;

#[derive(Debug, Clone, Copy)]
pub struct ExpandConfig {
    /// Default `max_visits` for SCC entry nodes that declare none. Sourced from
    /// `roki.toml [engine].max_iterations`.
    pub default_max_iterations: u32,
}

impl Default for ExpandConfig {
    fn default() -> Self {
        Self {
            default_max_iterations: 50,
        }
    }
}

#[derive(Debug, Error)]
pub enum ExpandError {
    #[error("rule[{rule_idx}] sugar entry has empty tasks list")]
    EmptyTasks { rule_idx: usize },
    #[error(
        "rule[{rule_idx}] entry has no body content (no tasks, no states/terminals, no immediate-delete shorthand applicable)"
    )]
    NoBody { rule_idx: usize },
    #[error("rule[{rule_idx}] invalid duration '{value}'")]
    InvalidDuration { rule_idx: usize, value: String },
    #[error("validation errors after expansion")]
    Validation(Vec<validate::ValidationError>),
}

/// Top-level expansion entry point. Consumes `RawWorkflow` (the parser's IR)
/// and produces a fully-canonical `WorkflowFile`.
pub fn expand(raw: RawWorkflow, config: ExpandConfig) -> Result<WorkflowFile, ExpandError> {
    let admission = raw.admission.map(convert_admission);
    let rules = expand_rule_section(raw.rules, &config, "rules", false)?;
    let cleanup = expand_rule_section(raw.cleanup, &config, "cleanup", true)?;
    let on_failure = expand_rule_section(raw.on_failure, &config, "on_failure", false)?;

    let file = WorkflowFile {
        admission,
        rules,
        cleanup,
        on_failure,
    };
    validate::run(&file).map_err(ExpandError::Validation)?;
    Ok(file)
}

fn expand_rule_section(
    raw: Vec<RawRuleEntry>,
    cfg: &ExpandConfig,
    _section: &'static str,
    cleanup_allows_empty: bool,
) -> Result<Vec<RuleEntry>, ExpandError> {
    raw.into_iter()
        .enumerate()
        .map(|(rule_idx, r)| {
            let when = r.when.map(convert_when);
            let state_machine = match r.body {
                RawRuleBody::Empty {} if cleanup_allows_empty => immediate_delete_state_machine(),
                RawRuleBody::Empty {} => return Err(ExpandError::NoBody { rule_idx }),
                other => expand_body(other, rule_idx, cfg)?,
            };
            Ok(RuleEntry {
                when,
                state_machine,
            })
        })
        .collect()
}

/// Cleanup immediate-delete shorthand: a state machine with one terminal
/// already at start. The cycle never executes a state and the engine deletes
/// the worktree synchronously.
fn immediate_delete_state_machine() -> StateMachine {
    let mut terminals = BTreeMap::new();
    terminals.insert(
        "__success__".to_string(),
        Terminal {
            id: "__success__".to_string(),
            outcome: "cleaned".to_string(),
        },
    );
    StateMachine {
        start: "__success__".to_string(),
        states: BTreeMap::new(),
        terminals,
    }
}

fn expand_body(
    body: RawRuleBody,
    rule_idx: usize,
    cfg: &ExpandConfig,
) -> Result<StateMachine, ExpandError> {
    let mut sm = match body {
        RawRuleBody::Empty {} => return Err(ExpandError::NoBody { rule_idx }),
        RawRuleBody::Canonical {
            start,
            states,
            terminals,
            ..
        } => StateMachine {
            start,
            states: convert_states(states, rule_idx)?,
            terminals: convert_terminals(terminals),
        },
        RawRuleBody::Sugar {
            tasks,
            states,
            terminals,
            on_fail,
        } => build_sugar_state_machine(tasks, states, terminals, on_fail, rule_idx)?,
    };

    apply_pass1_implicit_terminals(&mut sm);
    apply_pass5_max_visits(&mut sm, cfg.default_max_iterations);
    classify_edges(&mut sm);
    Ok(sm)
}

fn build_sugar_state_machine(
    tasks: Vec<RawTaskEntry>,
    states_inline: BTreeMap<StateId, RawStateEntry>,
    terminals: BTreeMap<StateId, RawTerminalEntry>,
    rule_on_fail: Option<StateId>,
    rule_idx: usize,
) -> Result<StateMachine, ExpandError> {
    if tasks.is_empty() {
        return Err(ExpandError::EmptyTasks { rule_idx });
    }
    let start = tasks[0].id.clone();
    let mut states = convert_states(states_inline, rule_idx)?;
    for (i, task) in tasks.iter().enumerate() {
        let next_id_opt = tasks.get(i + 1).map(|t| t.id.clone());
        let s = task_to_state(task, next_id_opt, rule_on_fail.as_deref(), rule_idx)?;
        states.insert(s.id.clone(), s);
    }
    Ok(StateMachine {
        start,
        states,
        terminals: convert_terminals(terminals),
    })
}

fn task_to_state(
    task: &RawTaskEntry,
    next_id: Option<StateId>,
    rule_on_fail: Option<&str>,
    rule_idx: usize,
) -> Result<State, ExpandError> {
    let on_done = match next_id {
        Some(id) => EdgeTarget::State(id),
        None => EdgeTarget::Terminal("__success__".to_string()),
    };
    let on_fail = match task
        .on_fail
        .clone()
        .or_else(|| rule_on_fail.map(String::from))
    {
        Some(id) => EdgeTarget::State(id),
        None => EdgeTarget::Terminal("__failure__".to_string()),
    };
    let body = body_from_run_uses(task.run.as_deref(), task.uses.as_deref());
    let timeout = parse_duration(task.timeout.as_deref(), rule_idx)?;
    Ok(State {
        id: task.id.clone(),
        body,
        if_cond: task.if_cond.clone(),
        on_done,
        on_fail,
        directives: convert_directives(&task.directives),
        max_visits: task.max_visits.unwrap_or(1),
        timeout,
    })
}

fn convert_states(
    raw: BTreeMap<StateId, RawStateEntry>,
    rule_idx: usize,
) -> Result<BTreeMap<StateId, State>, ExpandError> {
    let mut out = BTreeMap::new();
    for (id, raw_state) in raw {
        let body = body_from_run_uses(raw_state.run.as_deref(), raw_state.uses.as_deref());
        let timeout = parse_duration(raw_state.timeout.as_deref(), rule_idx)?;
        let on_done = raw_state
            .on_done
            .map(EdgeTarget::State)
            .unwrap_or(EdgeTarget::Terminal("__success__".to_string()));
        let on_fail = raw_state
            .on_fail
            .map(EdgeTarget::State)
            .unwrap_or(EdgeTarget::Terminal("__failure__".to_string()));
        let s = State {
            id: id.clone(),
            body,
            if_cond: raw_state.if_cond,
            on_done,
            on_fail,
            directives: convert_directives(&raw_state.directives),
            max_visits: raw_state.max_visits.unwrap_or(1),
            timeout,
        };
        out.insert(id, s);
    }
    Ok(out)
}

fn convert_terminals(raw: BTreeMap<StateId, RawTerminalEntry>) -> BTreeMap<StateId, Terminal> {
    raw.into_iter()
        .map(|(id, t)| {
            (
                id.clone(),
                Terminal {
                    id,
                    outcome: t.outcome,
                },
            )
        })
        .collect()
}

fn convert_directives(
    raw: &BTreeMap<DirectiveName, RawDirectiveTarget>,
) -> BTreeMap<DirectiveName, EdgeTarget> {
    raw.iter()
        .map(|(name, t)| {
            let target_id = match t {
                RawDirectiveTarget::Short(id) => id.clone(),
                RawDirectiveTarget::Long { target, .. } => target.clone(),
            };
            (name.clone(), EdgeTarget::State(target_id))
        })
        .collect()
}

fn body_from_run_uses(run: Option<&str>, uses: Option<&std::path::Path>) -> StateBody {
    match (run, uses) {
        (Some(cmd), None) => StateBody::Run {
            cmd: cmd.to_string(),
        },
        (None, Some(path)) => StateBody::Uses {
            path: path.to_path_buf(),
        },
        // Both or neither — validation will flag. Stub to keep struct buildable.
        _ => StateBody::Run { cmd: String::new() },
    }
}

fn parse_duration(s: Option<&str>, rule_idx: usize) -> Result<Option<Duration>, ExpandError> {
    let Some(value) = s else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if let Some(num) = trimmed.strip_suffix("ms") {
        num.trim()
            .parse::<u64>()
            .map(Duration::from_millis)
            .map(Some)
            .map_err(|_| ExpandError::InvalidDuration {
                rule_idx,
                value: value.to_string(),
            })
    } else if let Some(num) = trimmed.strip_suffix('m') {
        num.trim()
            .parse::<u64>()
            .map(|n| Duration::from_secs(n * 60))
            .map(Some)
            .map_err(|_| ExpandError::InvalidDuration {
                rule_idx,
                value: value.to_string(),
            })
    } else if let Some(num) = trimmed.strip_suffix('s') {
        num.trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map(Some)
            .map_err(|_| ExpandError::InvalidDuration {
                rule_idx,
                value: value.to_string(),
            })
    } else {
        // Bare integer = seconds.
        trimmed
            .parse::<u64>()
            .map(Duration::from_secs)
            .map(Some)
            .map_err(|_| ExpandError::InvalidDuration {
                rule_idx,
                value: value.to_string(),
            })
    }
}

fn convert_admission(raw: RawAdmission) -> Admission {
    Admission {
        assignee: raw.assignee,
        repos: raw.repos.into_iter().map(convert_repo).collect(),
    }
}

fn convert_repo(raw: RawRepoEntry) -> RepoEntry {
    RepoEntry {
        ghq: raw.ghq,
        when: raw.when.map(convert_when),
        workflow: raw.workflow,
    }
}

fn convert_when(raw: RawWhenClause) -> WhenClause {
    WhenClause {
        status: raw.status.map(convert_scalar_matcher),
        labels: raw.labels.map(convert_labels_matcher),
        assignee: raw.assignee.map(convert_scalar_matcher),
        repo: raw.repo,
        kind: raw.kind.map(convert_scalar_matcher),
        phase: raw.phase.map(convert_scalar_matcher),
        title: raw.title.map(convert_text_matcher),
        body: raw.body.map(convert_text_matcher),
    }
}

fn convert_scalar_matcher(raw: RawScalarMatcher) -> ScalarMatcher {
    match raw {
        RawScalarMatcher::Eq(s) => ScalarMatcher::Eq(s),
        RawScalarMatcher::Op { not: Some(s), .. } => ScalarMatcher::Not(s),
        RawScalarMatcher::Op { in_: Some(v), .. } => ScalarMatcher::In(v),
        RawScalarMatcher::Op { .. } => ScalarMatcher::Eq(String::new()),
    }
}

fn convert_labels_matcher(raw: RawLabelsMatcher) -> LabelsMatcher {
    LabelsMatcher {
        has_all: raw.has_all,
        has_any: raw.has_any,
        has_none: raw.has_none,
    }
}

fn convert_text_matcher(raw: RawTextMatcher) -> TextMatcher {
    if let Some(s) = raw.regex {
        TextMatcher::Regex(s)
    } else if let Some(s) = raw.starts_with {
        TextMatcher::StartsWith(s)
    } else if let Some(s) = raw.contains {
        TextMatcher::Contains(s)
    } else {
        TextMatcher::Contains(String::new())
    }
}

// -- Pass 1: implicit terminals --

fn apply_pass1_implicit_terminals(sm: &mut StateMachine) {
    let referenced = collect_referenced_targets(sm);
    for builtin in [
        "__success__",
        "__failure__",
        "__no_action__",
        "__cancelled__",
    ] {
        if referenced.contains(builtin) && !sm.terminals.contains_key(builtin) {
            sm.terminals.insert(
                builtin.to_string(),
                Terminal {
                    id: builtin.to_string(),
                    outcome: default_outcome_for(builtin).to_string(),
                },
            );
        }
    }
}

fn default_outcome_for(id: &str) -> &str {
    match id {
        "__success__" => "success",
        "__failure__" => "failure",
        "__no_action__" => "no_action",
        "__cancelled__" => "cancelled",
        _ => "",
    }
}

fn collect_referenced_targets(sm: &StateMachine) -> BTreeSet<StateId> {
    let mut out = BTreeSet::new();
    out.insert(sm.start.clone());
    for state in sm.states.values() {
        push_target(&mut out, &state.on_done);
        push_target(&mut out, &state.on_fail);
        for target in state.directives.values() {
            push_target(&mut out, target);
        }
    }
    out
}

fn push_target(out: &mut BTreeSet<StateId>, t: &EdgeTarget) {
    let id = match t {
        EdgeTarget::State(id) | EdgeTarget::Terminal(id) => id.clone(),
    };
    out.insert(id);
}

// -- Pass 5: auto-max_visits via Tarjan SCC --

fn apply_pass5_max_visits(sm: &mut StateMachine, default_cap: u32) {
    let sccs = tarjan_scc(sm);
    for scc in sccs {
        if scc.len() == 1 {
            let id = &scc[0];
            // Self-loop check: any edge points back to self?
            let has_self_edge = state_has_self_edge(sm, id);
            if !has_self_edge {
                continue;
            }
            inject_if_unset(sm, id, default_cap);
        } else {
            // Non-trivial SCC: pick lex-smallest id, inject if no member declares.
            let any_declared = scc
                .iter()
                .any(|id| sm.states.get(id).map(|s| s.max_visits > 1).unwrap_or(false));
            if any_declared {
                continue;
            }
            let mut sorted = scc.clone();
            sorted.sort();
            inject_if_unset(sm, &sorted[0], default_cap);
        }
    }
}

pub(crate) fn state_has_self_edge(sm: &StateMachine, id: &str) -> bool {
    let Some(state) = sm.states.get(id) else {
        return false;
    };
    matches!(&state.on_done, EdgeTarget::State(t) if t == id)
        || matches!(&state.on_fail, EdgeTarget::State(t) if t == id)
        || state
            .directives
            .values()
            .any(|t| matches!(t, EdgeTarget::State(s) if s == id))
}

fn inject_if_unset(sm: &mut StateMachine, id: &str, default_cap: u32) {
    if let Some(state) = sm.states.get_mut(id) {
        if state.max_visits <= 1 {
            state.max_visits = default_cap;
        }
    }
}

/// Tarjan strongly-connected components over `sm.states` only (terminals are
/// sinks). Returns SCCs sorted by reverse postorder.
pub(crate) fn tarjan_scc(sm: &StateMachine) -> Vec<Vec<StateId>> {
    let ids: Vec<StateId> = sm.states.keys().cloned().collect();
    let id_to_idx: BTreeMap<StateId, usize> = ids
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();

    let mut state = TarjanState {
        index: 0,
        stack: Vec::new(),
        on_stack: vec![false; ids.len()],
        indices: vec![None; ids.len()],
        lowlinks: vec![0; ids.len()],
        sccs: Vec::new(),
        ids: &ids,
    };

    for v in 0..ids.len() {
        if state.indices[v].is_none() {
            tarjan_strongconnect(&mut state, sm, &id_to_idx, v);
        }
    }
    state.sccs
}

struct TarjanState<'a> {
    index: usize,
    stack: Vec<usize>,
    on_stack: Vec<bool>,
    indices: Vec<Option<usize>>,
    lowlinks: Vec<usize>,
    sccs: Vec<Vec<StateId>>,
    ids: &'a [StateId],
}

fn tarjan_strongconnect(
    s: &mut TarjanState,
    sm: &StateMachine,
    id_to_idx: &BTreeMap<StateId, usize>,
    v: usize,
) {
    s.indices[v] = Some(s.index);
    s.lowlinks[v] = s.index;
    s.index += 1;
    s.stack.push(v);
    s.on_stack[v] = true;

    let v_id = &s.ids[v];
    let neighbors = state_state_neighbors(sm, v_id, id_to_idx);
    for w in neighbors {
        if s.indices[w].is_none() {
            tarjan_strongconnect(s, sm, id_to_idx, w);
            s.lowlinks[v] = s.lowlinks[v].min(s.lowlinks[w]);
        } else if s.on_stack[w] {
            s.lowlinks[v] = s.lowlinks[v].min(s.indices[w].unwrap());
        }
    }

    if s.lowlinks[v] == s.indices[v].unwrap() {
        let mut scc = Vec::new();
        loop {
            let w = s.stack.pop().unwrap();
            s.on_stack[w] = false;
            scc.push(s.ids[w].clone());
            if w == v {
                break;
            }
        }
        s.sccs.push(scc);
    }
}

fn state_state_neighbors(
    sm: &StateMachine,
    id: &str,
    id_to_idx: &BTreeMap<StateId, usize>,
) -> Vec<usize> {
    let Some(state) = sm.states.get(id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push = |t: &EdgeTarget| {
        if let EdgeTarget::State(target_id) = t {
            if let Some(&idx) = id_to_idx.get(target_id) {
                out.push(idx);
            }
        }
    };
    push(&state.on_done);
    push(&state.on_fail);
    for target in state.directives.values() {
        push(target);
    }
    out
}

// -- Edge classification: State vs Terminal --

fn classify_edges(sm: &mut StateMachine) {
    let terminal_ids: BTreeSet<StateId> = sm.terminals.keys().cloned().collect();
    for state in sm.states.values_mut() {
        reclassify(&mut state.on_done, &terminal_ids);
        reclassify(&mut state.on_fail, &terminal_ids);
        for target in state.directives.values_mut() {
            reclassify(target, &terminal_ids);
        }
    }
}

fn reclassify(t: &mut EdgeTarget, terminals: &BTreeSet<StateId>) {
    if let EdgeTarget::State(id) = t {
        if terminals.contains(id) {
            *t = EdgeTarget::Terminal(id.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::parse;
    use std::path::Path;

    fn expand_yaml(yaml: &str) -> Result<WorkflowFile, ExpandError> {
        let raw = parse::parse_workflow_str(Path::new("WORKFLOW.yaml"), yaml).unwrap();
        expand(raw, ExpandConfig::default())
    }

    #[test]
    fn minimal_sugar_one_task() {
        let yaml = r#"
admission: { assignee: me, repos: [{ ghq: x }] }
rules:
  - tasks:
      - id: impl
        run: echo hi
"#;
        let f = expand_yaml(yaml).unwrap();
        let sm = &f.rules[0].state_machine;
        assert_eq!(sm.start, "impl");
        assert_eq!(sm.states.len(), 1);
        let impl_state = &sm.states["impl"];
        assert_eq!(
            impl_state.on_done,
            EdgeTarget::Terminal("__success__".into())
        );
        assert_eq!(
            impl_state.on_fail,
            EdgeTarget::Terminal("__failure__".into())
        );
        assert!(sm.terminals.contains_key("__success__"));
        assert!(sm.terminals.contains_key("__failure__"));
    }

    #[test]
    fn sugar_chain_links_tasks_in_order() {
        let yaml = r#"
admission: { assignee: me, repos: [{ ghq: x }] }
rules:
  - tasks:
      - { id: judge, run: j }
      - { id: impl, run: i }
      - { id: verdict, run: v }
"#;
        let sm = &expand_yaml(yaml).unwrap().rules[0].state_machine;
        assert_eq!(sm.start, "judge");
        assert_eq!(sm.states["judge"].on_done, EdgeTarget::State("impl".into()));
        assert_eq!(
            sm.states["impl"].on_done,
            EdgeTarget::State("verdict".into())
        );
        assert_eq!(
            sm.states["verdict"].on_done,
            EdgeTarget::Terminal("__success__".into())
        );
    }

    #[test]
    fn sugar_retry_directive_self_loop_injects_max_visits() {
        let yaml = r#"
admission: { assignee: me, repos: [{ ghq: x }] }
rules:
  - tasks:
      - { id: a, run: x }
      - id: b
        run: y
        directives:
          retry: a
"#;
        let sm = &expand_yaml(yaml).unwrap().rules[0].state_machine;
        // SCC contains {a, b} since b→a via directive and a→b via on_done chain.
        // Lex-smallest is "a" → max_visits = default (50).
        assert_eq!(sm.states["a"].max_visits, 50);
        assert_eq!(sm.states["b"].max_visits, 1);
    }

    #[test]
    fn directive_targets_terminal_classified_correctly() {
        let yaml = r#"
admission: { assignee: me, repos: [{ ghq: x }] }
rules:
  - start: a
    states:
      a:
        run: x
        directives:
          skip: __no_action__
    terminals: {}
"#;
        let sm = &expand_yaml(yaml).unwrap().rules[0].state_machine;
        assert_eq!(
            sm.states["a"].directives["skip"],
            EdgeTarget::Terminal("__no_action__".into())
        );
        // Auto-injected.
        assert_eq!(sm.terminals["__no_action__"].outcome, "no_action");
    }

    #[test]
    fn cleanup_immediate_delete_yields_terminal_at_start() {
        let yaml = r#"
admission: { assignee: me, repos: [{ ghq: x }] }
cleanup:
  - when: { status: Done }
"#;
        let sm = &expand_yaml(yaml).unwrap().cleanup[0].state_machine;
        assert!(sm.states.is_empty());
        assert_eq!(sm.start, "__success__");
        assert!(sm.terminals.contains_key("__success__"));
    }

    #[test]
    fn duration_parser_handles_ms_s_m_and_bare_seconds() {
        assert_eq!(
            parse_duration(Some("500ms"), 0).unwrap(),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            parse_duration(Some("30s"), 0).unwrap(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_duration(Some("5m"), 0).unwrap(),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            parse_duration(Some("60"), 0).unwrap(),
            Some(Duration::from_secs(60))
        );
        assert!(parse_duration(Some("garbage"), 0).is_err());
    }

    #[test]
    fn explicit_max_visits_in_scc_preserved() {
        let yaml = r#"
admission: { assignee: me, repos: [{ ghq: x }] }
rules:
  - tasks:
      - { id: a, run: x, max_visits: 3 }
      - id: b
        run: y
        directives:
          retry: a
"#;
        let sm = &expand_yaml(yaml).unwrap().rules[0].state_machine;
        // Operator-declared 3 wins; no auto-injection.
        assert_eq!(sm.states["a"].max_visits, 3);
    }

    #[test]
    fn implicit_terminals_injected_only_when_referenced() {
        let yaml = r#"
admission: { assignee: me, repos: [{ ghq: x }] }
rules:
  - tasks:
      - { id: a, run: x }
"#;
        let sm = &expand_yaml(yaml).unwrap().rules[0].state_machine;
        assert!(sm.terminals.contains_key("__success__"));
        assert!(sm.terminals.contains_key("__failure__"));
        assert!(!sm.terminals.contains_key("__cancelled__"));
        assert!(!sm.terminals.contains_key("__no_action__"));
    }
}
