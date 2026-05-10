//! Cycle dispatch evaluator: cleanup-first then rule first-match.
//!
//! Spec: fr:01 §38 + fr:07 §Cycle dispatch.
//!
//! Slice 8 produces canonical `RuleEntry` references. The cleanup
//! shorthand is detected via `rule::is_shorthand_cleanup`.

#![allow(dead_code)]

use crate::admission::AdmittedTicket;
use crate::config::workflow::WorkflowConfig;
use crate::engine::outcome::CycleKind;
use crate::workflow::canonical::RuleEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Default: evaluate cleanup first, then rules.
    Default,
    /// `roki cleanup` subcommand: only cleanup matches lead to a cycle.
    CleanupOnly,
}

#[derive(Debug)]
pub enum DispatchTarget<'a> {
    /// Spawn a normal cycle (rule or cleanup) running this entry's state
    /// machine.
    Cycle {
        kind: CycleKind,
        rule: &'a RuleEntry,
    },
    /// Cleanup shorthand: synchronous delete, no cycle.
    CleanupShorthand,
    /// No cleanup and no rule matched.
    NoMatch,
}

pub fn evaluate<'a>(
    admitted: &AdmittedTicket,
    workflow: &'a WorkflowConfig,
    mode: DispatchMode,
) -> DispatchTarget<'a> {
    if let Some(c) = crate::rule::first_cleanup_match(admitted, &workflow.cleanups) {
        if crate::rule::is_shorthand_cleanup(c) {
            return DispatchTarget::CleanupShorthand;
        }
        return DispatchTarget::Cycle {
            kind: CycleKind::Cleanup,
            rule: c,
        };
    }

    if matches!(mode, DispatchMode::CleanupOnly) {
        return DispatchTarget::NoMatch;
    }

    if let Some(r) = crate::rule::first_match(admitted, &workflow.rules) {
        return DispatchTarget::Cycle {
            kind: CycleKind::Rule,
            rule: r,
        };
    }

    DispatchTarget::NoMatch
}

/// Same as `evaluate`, but synthesizes the `AdmittedTicket` from a cache
/// snapshot (per-ticket task path).
pub fn evaluate_from_cache<'a>(
    ticket_id: &str,
    snap: &crate::daemon::cache::CacheEntry,
    workflow: &'a WorkflowConfig,
    mode: DispatchMode,
) -> DispatchTarget<'a> {
    let synthetic = AdmittedTicket {
        ticket: crate::linear::ticket::NormalizedTicket::new(
            ticket_id.to_string(),
            Some(snap.assignee.clone()),
            snap.status.clone(),
            snap.labels.iter().cloned().collect(),
            String::new(),
            String::new(),
        ),
        ghq: snap.repo.clone(),
    };
    evaluate(&synthetic, workflow, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::workflow::workflow_config_for_test;
    use crate::workflow::canonical::test_helpers as h;
    use crate::workflow::canonical::{
        LabelsMatcher, RuleEntry, ScalarMatcher, StateMachine, Terminal, WhenClause,
    };

    fn dummy_sm() -> StateMachine {
        let mut sm = h::state_machine();
        sm.start = "a".into();
        sm.states.insert("a".into(), h::state("a", "true"));
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "success".into(),
            },
        );
        sm
    }

    fn shorthand_sm() -> StateMachine {
        let mut sm = h::state_machine();
        sm.start = "__success__".into();
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "cleaned".into(),
            },
        );
        sm
    }

    fn rule_for(status: &str) -> RuleEntry {
        let mut when = WhenClause::default();
        when.status = Some(ScalarMatcher::Eq(status.into()));
        RuleEntry {
            when: Some(when),
            state_machine: dummy_sm(),
        }
    }

    fn cleanup_for(status: Option<&str>) -> RuleEntry {
        let when = status.map(|s| {
            let mut w = WhenClause::default();
            w.status = Some(ScalarMatcher::Eq(s.into()));
            w
        });
        RuleEntry {
            when,
            state_machine: dummy_sm(),
        }
    }

    fn shorthand_cleanup() -> RuleEntry {
        RuleEntry {
            when: None,
            state_machine: shorthand_sm(),
        }
    }

    #[test]
    fn cleanup_wins_over_rule() {
        let wf = workflow_config_for_test(
            "u1",
            Some("github.com/acme/widget"),
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("InProgress"))],
            vec![],
        );
        let a = crate::rule::admitted_with("InProgress", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::Cycle {
                kind: CycleKind::Cleanup,
                ..
            } => {}
            other => panic!("expected Cleanup cycle, got {other:?}"),
        }
    }

    #[test]
    fn shorthand_dispatch() {
        let wf = workflow_config_for_test(
            "u1",
            Some("github.com/acme/widget"),
            vec![rule_for("Done")],
            vec![shorthand_cleanup()],
            vec![],
        );
        let a = crate::rule::admitted_with("Done", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::CleanupShorthand => {}
            other => panic!("expected CleanupShorthand, got {other:?}"),
        }
    }

    #[test]
    fn rule_dispatch_when_no_cleanup_match() {
        let wf = workflow_config_for_test(
            "u1",
            Some("github.com/acme/widget"),
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("Done"))],
            vec![],
        );
        let a = crate::rule::admitted_with("InProgress", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::Cycle {
                kind: CycleKind::Rule,
                ..
            } => {}
            other => panic!("expected Rule cycle, got {other:?}"),
        }
    }

    #[test]
    fn no_match_when_neither_list_hits() {
        let wf = workflow_config_for_test(
            "u1",
            Some("github.com/acme/widget"),
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("Done"))],
            vec![],
        );
        let a = crate::rule::admitted_with("Triage", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::NoMatch => {}
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_only_mode_ignores_rule_list() {
        let wf = workflow_config_for_test(
            "u1",
            Some("github.com/acme/widget"),
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("Done"))],
            vec![],
        );
        let a = crate::rule::admitted_with("InProgress", vec![]);
        match evaluate(&a, &wf, DispatchMode::CleanupOnly) {
            DispatchTarget::NoMatch => {}
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    use crate::daemon::cache::CacheEntry;
    use std::collections::BTreeSet;
    use time::OffsetDateTime;

    fn snapshot_for(status: &str, labels: &[&str]) -> CacheEntry {
        CacheEntry {
            repo: "github.com/example/repo".into(),
            workflow_path: None,
            status: status.into(),
            labels: labels
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>(),
            assignee: "u1".into(),
            cycle_id: None,
            pending_recheck: false,
            pending_evict: false,
            last_event_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn evaluate_from_cache_dispatches_cleanup_first() {
        let wf = workflow_config_for_test(
            "u1",
            Some("github.com/example/repo"),
            vec![rule_for("Done")],
            vec![cleanup_for(Some("Done"))],
            vec![],
        );
        let snap = snapshot_for("Done", &[]);
        match evaluate_from_cache("t1", &snap, &wf, DispatchMode::Default) {
            DispatchTarget::Cycle {
                kind: CycleKind::Cleanup,
                ..
            } => {}
            other => panic!("expected Cleanup cycle, got {other:?}"),
        }
    }

    #[test]
    fn _import_labels_for_use() {
        let _: LabelsMatcher = LabelsMatcher::default();
    }
}
