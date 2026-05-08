//! Cycle dispatch evaluator: cleanup-first then rule first-match.
//! Per fr:01 §38 + fr:07 §Cycle dispatch.

#![allow(dead_code)]

use crate::admission::AdmittedTicket;
use crate::config::workflow::{Cleanup, Rule, WorkflowConfig};
use crate::engine::outcome::CycleKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Default: evaluate `[[cleanup]]` first, then `[[rule]]`.
    Default,
    /// `roki cleanup` subcommand: only `[[cleanup]]` matches lead to a cycle.
    /// `[[rule]]` list is ignored.
    CleanupOnly,
}

#[derive(Debug)]
pub enum DispatchTarget<'a> {
    /// Spawn a normal cycle (rule or cleanup) with these phases.
    Cycle {
        kind: CycleKind,
        rule: Option<&'a Rule>,
        cleanup: Option<&'a Cleanup>,
    },
    /// Cleanup shorthand: synchronous delete, no cycle.
    CleanupShorthand,
    /// No `[[cleanup]]` and no `[[rule]]` matched.
    NoMatch,
}

pub fn evaluate<'a>(
    admitted: &AdmittedTicket,
    workflow: &'a WorkflowConfig,
    mode: DispatchMode,
) -> DispatchTarget<'a> {
    if let Some(c) = crate::rule::first_cleanup_match(admitted, &workflow.cleanups) {
        if c.is_shorthand() {
            return DispatchTarget::CleanupShorthand;
        }
        return DispatchTarget::Cycle {
            kind: CycleKind::Cleanup,
            rule: None,
            cleanup: Some(c),
        };
    }

    if matches!(mode, DispatchMode::CleanupOnly) {
        return DispatchTarget::NoMatch;
    }

    if let Some(r) = crate::rule::first_match(admitted, &workflow.rules) {
        return DispatchTarget::Cycle {
            kind: CycleKind::Rule,
            rule: Some(r),
            cleanup: None,
        };
    }

    DispatchTarget::NoMatch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::workflow::{Cleanup, Rule};
    use crate::engine::outcome::PhaseBody;

    fn workflow_with(rules: Vec<Rule>, cleanups: Vec<Cleanup>) -> WorkflowConfig {
        WorkflowConfig {
            admission: crate::config::workflow::AdmissionSection {
                assignee: "me".into(),
            },
            repo: None,
            rules,
            cleanups,
            on_failures: vec![],
        }
    }

    fn rule_for(status: &str) -> Rule {
        Rule {
            when_status: status.into(),
            when_labels_has_all: vec![],
            pre: None,
            run: PhaseBody::InlineCmd { cmd: "true".into() },
            post: None,
        }
    }

    fn cleanup_for(status: Option<&str>) -> Cleanup {
        Cleanup {
            when_status: status.map(String::from),
            when_labels_has_all: vec![],
            pre: None,
            run: status.map(|_| PhaseBody::InlineCmd { cmd: "true".into() }),
            post: None,
        }
    }

    fn shorthand_cleanup() -> Cleanup {
        Cleanup {
            when_status: None,
            when_labels_has_all: vec![],
            pre: None,
            run: None,
            post: None,
        }
    }

    #[test]
    fn cleanup_wins_over_rule() {
        let wf = workflow_with(
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("InProgress"))],
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
        let wf = workflow_with(vec![rule_for("Done")], vec![shorthand_cleanup()]);
        let a = crate::rule::admitted_with("Done", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::CleanupShorthand => {}
            other => panic!("expected CleanupShorthand, got {other:?}"),
        }
    }

    #[test]
    fn rule_dispatch_when_no_cleanup_match() {
        let wf = workflow_with(
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("Done"))],
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
        let wf = workflow_with(
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("Done"))],
        );
        let a = crate::rule::admitted_with("Triage", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::NoMatch => {}
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_only_mode_ignores_rule_list() {
        let wf = workflow_with(
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("Done"))],
        );
        let a = crate::rule::admitted_with("InProgress", vec![]);
        match evaluate(&a, &wf, DispatchMode::CleanupOnly) {
            DispatchTarget::NoMatch => {}
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }
}
