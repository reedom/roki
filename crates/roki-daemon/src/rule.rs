// Walking-skeleton tasks land in dependency order: this evaluator (task 4.2)
// precedes the runtime wiring that calls `Rule::first_match` per cycle. Until
// that wiring lands, the function is exercised only by the unit tests below,
// which triggers `dead_code` for the leaf API. Allow it module-locally instead
// of leaking the relaxation crate-wide, matching the pattern in `admission`
// and `config::workflow`.
#![allow(dead_code)]

//! First-match rule evaluator for the walking-skeleton daemon.
//!
//! Pure function over an `AdmittedTicket` and the loaded `[[rule]]` array.
//! Evaluates rules in declared order and returns the first whose declared
//! `when.status` equals the ticket's status and whose declared
//! `when.labels.has_all` is fully contained in the ticket's labels (Req 5.1,
//! 5.2). `[[cleanup]]` and `[[on_failure]]` lists are not evaluated here
//! (Req 5.5); a `None` return represents the no-match outcome the runtime
//! surfaces as an info-level log without spawning a cycle (Req 5.4).
//!
//! Configuration-time validation (rejecting unsupported `when.*` keys per
//! Req 5.3) is the responsibility of `config::workflow`; by the time a
//! `&[Rule]` reaches this evaluator the shape is already restricted to
//! `when.status` + `when.labels.has_all` + `run.cmd`.

use std::collections::HashSet;

use crate::admission::AdmittedTicket;
use crate::config::workflow::Rule;

/// Return the first rule whose `when.status` equals the ticket's status and
/// whose `when.labels.has_all` is fully contained in the ticket's labels, or
/// `None` when no entry matches.
///
/// Rules are scanned in declared order; iteration short-circuits on the first
/// hit so later entries are never reached when an earlier rule matches
/// (Req 5.1).
pub fn first_match<'a>(
    admitted: &AdmittedTicket,
    rules: &'a [Rule],
) -> Option<&'a Rule> {
    let labels: HashSet<&str> =
        admitted.ticket.labels.iter().map(String::as_str).collect();
    rules.iter().find(|rule| matches(admitted, &labels, rule))
}

/// String equality on `when.status` plus set containment on
/// `when.labels.has_all` against the ticket's labels (Req 5.2). An empty
/// `has_all` trivially matches because every element of the empty set is
/// already contained in any superset.
fn matches(
    admitted: &AdmittedTicket,
    ticket_labels: &HashSet<&str>,
    rule: &Rule,
) -> bool {
    if admitted.ticket.status != rule.when_status {
        return false;
    }
    rule.when_labels_has_all
        .iter()
        .all(|wanted| ticket_labels.contains(wanted.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::ticket::NormalizedTicket;

    fn admitted(status: &str, labels: &[&str]) -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "tid-1".to_string(),
                Some("u1".to_string()),
                status.to_string(),
                labels.iter().map(|s| s.to_string()).collect(),
                String::new(),
                String::new(),
            ),
            ghq: "github.com/owner/repo".to_string(),
        }
    }

    fn rule(status: &str, has_all: &[&str], cmd: &str) -> Rule {
        Rule {
            when_status: status.to_string(),
            when_labels_has_all: has_all.iter().map(|s| s.to_string()).collect(),
            pre: None,
            run: crate::engine::outcome::PhaseBody::InlineCmd { cmd: cmd.to_string() },
            post: None,
        }
    }

    #[test]
    fn matches_when_status_eq_and_has_all_contained() {
        let t = admitted("In Progress", &["bug", "p0"]);
        let rules = vec![rule("In Progress", &["bug"], "echo a")];

        let hit = first_match(&t, &rules)
            .expect("rule must match on status + has_all containment");
        match &hit.run {
            crate::engine::outcome::PhaseBody::InlineCmd { cmd } => assert_eq!(cmd, "echo a"),
            other => panic!("expected InlineCmd, got {other:?}"),
        }
    }

    #[test]
    fn returns_none_when_status_mismatches() {
        let t = admitted("Done", &["bug"]);
        let rules = vec![rule("In Progress", &["bug"], "echo a")];

        assert!(first_match(&t, &rules).is_none());
    }

    #[test]
    fn returns_none_when_has_all_not_contained() {
        // Ticket carries a strict subset of the rule's `has_all` requirement.
        let t = admitted("In Progress", &["bug"]);
        let rules = vec![rule("In Progress", &["bug", "p0"], "echo a")];

        assert!(first_match(&t, &rules).is_none());
    }

    #[test]
    fn first_matching_rule_wins_later_rules_unreachable() {
        // Both rules would match on their own; declared order must decide
        // (Req 5.1). The second rule must never be returned here.
        let t = admitted("In Progress", &["bug", "p0"]);
        let rules = vec![
            rule("In Progress", &["bug"], "echo first"),
            rule("In Progress", &["p0"], "echo second"),
        ];

        let hit = first_match(&t, &rules).expect("first rule must match");
        match &hit.run {
            crate::engine::outcome::PhaseBody::InlineCmd { cmd } => assert_eq!(cmd, "echo first"),
            other => panic!("expected InlineCmd, got {other:?}"),
        }
    }

    #[test]
    fn empty_has_all_matches_status_only() {
        // Vacuous truth: the empty `has_all` set is contained in any label
        // superset, so a rule with no required labels matches purely on
        // status equality.
        let t = admitted("In Progress", &[]);
        let rules = vec![rule("In Progress", &[], "echo a")];

        assert!(first_match(&t, &rules).is_some());
    }

    #[test]
    fn empty_rule_array_returns_none() {
        // Skeleton's no-match path (Req 5.4): the runtime turns `None` into
        // an info-level no-match outcome without spawning a cycle.
        let t = admitted("In Progress", &["bug"]);
        let rules: Vec<Rule> = Vec::new();

        assert!(first_match(&t, &rules).is_none());
    }

    #[test]
    fn skips_non_matching_status_then_finds_later_match() {
        // Earlier non-matching entry must not short-circuit before a later
        // legitimate match is reached.
        let t = admitted("In Progress", &["bug"]);
        let rules = vec![
            rule("Done", &["bug"], "echo skipped"),
            rule("In Progress", &["bug"], "echo hit"),
        ];

        let hit = first_match(&t, &rules).expect("second rule must match");
        match &hit.run {
            crate::engine::outcome::PhaseBody::InlineCmd { cmd } => assert_eq!(cmd, "echo hit"),
            other => panic!("expected InlineCmd, got {other:?}"),
        }
    }
}