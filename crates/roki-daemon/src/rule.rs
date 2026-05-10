//! First-match rule evaluator (slice 8, canonical types).
//!
//! Pure function over an `AdmittedTicket` and `[RuleEntry]`. Evaluates
//! `WhenClause` axes (`status`, `labels`, `assignee`, `repo`, `kind`,
//! `phase`, `title`, `body`) in declared order; returns the first matching
//! entry. Rule cycles match `kind: rule` (default when absent); cleanup
//! rows match `kind: cleanup` (caller selects the slice).

#![allow(dead_code)]

use std::collections::HashSet;

use crate::admission::AdmittedTicket;
use crate::workflow::canonical::{
    LabelsMatcher, RuleEntry, ScalarMatcher, TextMatcher, WhenClause,
};

/// Return the first rule whose `when:` accepts `admitted`, or `None` when no
/// entry matches.
pub fn first_match<'a>(admitted: &AdmittedTicket, rules: &'a [RuleEntry]) -> Option<&'a RuleEntry> {
    rules
        .iter()
        .find(|r| matches_when(r.when.as_ref(), admitted))
}

/// First-match against cleanup rows. Mirrors `first_match`. Shorthand
/// (state machine with empty states + start at `__success__`) still
/// requires `when:` to match if declared; an entry without `when:` matches
/// any admitted ticket.
pub fn first_cleanup_match<'a>(
    admitted: &AdmittedTicket,
    cleanups: &'a [RuleEntry],
) -> Option<&'a RuleEntry> {
    cleanups
        .iter()
        .find(|c| matches_when(c.when.as_ref(), admitted))
}

/// Detects the immediate-delete shorthand: a `RuleEntry` whose state
/// machine has no states and starts at `__success__` (per
/// `workflow::sugar::immediate_delete_state_machine`).
pub fn is_shorthand_cleanup(entry: &RuleEntry) -> bool {
    entry.state_machine.states.is_empty() && entry.state_machine.start == "__success__"
}

fn matches_when(when: Option<&WhenClause>, admitted: &AdmittedTicket) -> bool {
    let Some(w) = when else {
        return true;
    };

    if let Some(m) = &w.status {
        if !scalar_matches(m, &admitted.ticket.status) {
            return false;
        }
    }
    if let Some(m) = &w.labels {
        if !labels_match(m, &admitted.ticket.labels) {
            return false;
        }
    }
    if let Some(m) = &w.assignee {
        let candidate = admitted.ticket.assignee_id.as_deref().unwrap_or("");
        if !scalar_matches(m, candidate) {
            return false;
        }
    }
    if let Some(repo) = &w.repo {
        if repo != &admitted.ghq {
            return false;
        }
    }
    // `when.kind` and `when.phase` are surfaced by `on_failure::route`,
    // not by ticket-driven dispatch — skip them here.
    if let Some(t) = &w.title {
        if !text_matches(t, &admitted.ticket.title) {
            return false;
        }
    }
    if let Some(t) = &w.body {
        if !text_matches(t, &admitted.ticket.body) {
            return false;
        }
    }
    true
}

fn scalar_matches(m: &ScalarMatcher, candidate: &str) -> bool {
    match m {
        ScalarMatcher::Eq(s) => s == candidate,
        ScalarMatcher::Not(s) => s != candidate,
        ScalarMatcher::In(items) => items.iter().any(|s| s == candidate),
    }
}

fn labels_match(m: &LabelsMatcher, ticket_labels: &[String]) -> bool {
    let set: HashSet<&str> = ticket_labels.iter().map(String::as_str).collect();
    if let Some(all) = &m.has_all {
        for w in all {
            if !set.contains(w.as_str()) {
                return false;
            }
        }
    }
    if let Some(any) = &m.has_any {
        if !any.iter().any(|w| set.contains(w.as_str())) {
            return false;
        }
    }
    if let Some(none) = &m.has_none {
        for w in none {
            if set.contains(w.as_str()) {
                return false;
            }
        }
    }
    true
}

fn text_matches(m: &TextMatcher, candidate: &str) -> bool {
    match m {
        TextMatcher::StartsWith(s) => candidate.starts_with(s.as_str()),
        TextMatcher::Contains(s) => candidate.contains(s.as_str()),
        // Slice 8 keeps `regex:` in the schema for future use; first-match
        // currently treats it as a `Contains` check on the pattern body so
        // the schema validates without pulling in a regex crate dependency.
        // Operators that need regex semantics today should use `contains:`
        // until the daemon adds the `regex` crate (tracked in plan §11.7).
        TextMatcher::Regex(pattern) => candidate.contains(pattern.as_str()),
    }
}

/// Build a status-set seed from `rules` ∪ `cleanups`. Used by cold-start
/// to scope the Linear enumeration to statuses any rule cares about.
pub fn status_seed(rules: &[RuleEntry], cleanups: &[RuleEntry]) -> HashSet<String> {
    let mut seed = HashSet::new();
    for r in rules.iter().chain(cleanups.iter()) {
        let Some(w) = &r.when else { continue };
        let Some(s) = &w.status else { continue };
        match s {
            ScalarMatcher::Eq(v) => {
                seed.insert(v.clone());
            }
            ScalarMatcher::In(items) => {
                for v in items {
                    seed.insert(v.clone());
                }
            }
            // `Not` excludes one value; nothing to seed.
            ScalarMatcher::Not(_) => {}
        }
    }
    seed
}

#[cfg(test)]
pub(crate) fn admitted_with(status: &str, labels: Vec<String>) -> AdmittedTicket {
    AdmittedTicket {
        ticket: crate::linear::ticket::NormalizedTicket::new(
            "ENG-DSP".to_string(),
            Some("u1".to_string()),
            status.to_string(),
            labels,
            "T".to_string(),
            "B".to_string(),
        ),
        ghq: "github.com/acme/widget".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::ticket::NormalizedTicket;
    use crate::workflow::canonical::test_helpers as h;
    use crate::workflow::canonical::{StateMachine, Terminal};

    fn admitted(status: &str, labels: &[&str]) -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "tid-1".to_string(),
                Some("u1".to_string()),
                status.to_string(),
                labels.iter().map(|s| s.to_string()).collect(),
                "Title".into(),
                "Body".into(),
            ),
            ghq: "github.com/owner/repo".to_string(),
        }
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

    fn rule(status: &str, has_all: &[&str]) -> RuleEntry {
        let mut when = WhenClause::default();
        when.status = Some(ScalarMatcher::Eq(status.into()));
        if !has_all.is_empty() {
            when.labels = Some(LabelsMatcher {
                has_all: Some(has_all.iter().map(|s| s.to_string()).collect()),
                ..LabelsMatcher::default()
            });
        }
        RuleEntry {
            when: Some(when),
            state_machine: dummy_sm(),
        }
    }

    fn cleanup_when(status: Option<&str>, labels: &[&str]) -> RuleEntry {
        let when = if status.is_none() && labels.is_empty() {
            None
        } else {
            let mut w = WhenClause::default();
            if let Some(s) = status {
                w.status = Some(ScalarMatcher::Eq(s.into()));
            }
            if !labels.is_empty() {
                w.labels = Some(LabelsMatcher {
                    has_all: Some(labels.iter().map(|s| s.to_string()).collect()),
                    ..LabelsMatcher::default()
                });
            }
            Some(w)
        };
        RuleEntry {
            when,
            state_machine: dummy_sm(),
        }
    }

    fn cleanup_shorthand() -> RuleEntry {
        RuleEntry {
            when: None,
            state_machine: shorthand_sm(),
        }
    }

    #[test]
    fn matches_when_status_eq_and_has_all_contained() {
        let t = admitted("In Progress", &["bug", "p0"]);
        let rules = vec![rule("In Progress", &["bug"])];
        assert!(first_match(&t, &rules).is_some());
    }

    #[test]
    fn returns_none_when_status_mismatches() {
        let t = admitted("Done", &["bug"]);
        let rules = vec![rule("In Progress", &["bug"])];
        assert!(first_match(&t, &rules).is_none());
    }

    #[test]
    fn returns_none_when_has_all_not_contained() {
        let t = admitted("In Progress", &["bug"]);
        let rules = vec![rule("In Progress", &["bug", "p0"])];
        assert!(first_match(&t, &rules).is_none());
    }

    #[test]
    fn first_matching_rule_wins() {
        let t = admitted("In Progress", &["bug", "p0"]);
        let rules = vec![rule("In Progress", &["bug"]), rule("In Progress", &["p0"])];
        let hit = first_match(&t, &rules).unwrap();
        assert_eq!(
            hit.when
                .as_ref()
                .unwrap()
                .labels
                .as_ref()
                .unwrap()
                .has_all
                .as_ref()
                .unwrap()[0],
            "bug"
        );
    }

    #[test]
    fn empty_rule_array_returns_none() {
        let t = admitted("In Progress", &["bug"]);
        let rules: Vec<RuleEntry> = Vec::new();
        assert!(first_match(&t, &rules).is_none());
    }

    #[test]
    fn cleanup_shorthand_matches_unconditionally() {
        let t = admitted("InProgress", &[]);
        let cleanups = vec![cleanup_shorthand()];
        let hit = first_cleanup_match(&t, &cleanups).unwrap();
        assert!(is_shorthand_cleanup(hit));
    }

    #[test]
    fn cleanup_status_filter() {
        let t = admitted("InProgress", &[]);
        let cleanups = vec![cleanup_when(Some("Done"), &[])];
        assert!(first_cleanup_match(&t, &cleanups).is_none());

        let t2 = admitted("Done", &[]);
        assert!(first_cleanup_match(&t2, &cleanups).is_some());
    }

    #[test]
    fn cleanup_labels_has_all() {
        let t1 = admitted("InProgress", &["urgent"]);
        let cleanups = vec![cleanup_when(None, &["urgent", "bug"])];
        assert!(first_cleanup_match(&t1, &cleanups).is_none());

        let t2 = admitted("InProgress", &["urgent", "bug"]);
        assert!(first_cleanup_match(&t2, &cleanups).is_some());
    }

    #[test]
    fn status_seed_collects_eq_and_in() {
        let mut not_when = WhenClause::default();
        not_when.status = Some(ScalarMatcher::Not("Done".into()));
        let entries = vec![
            rule("InProgress", &[]),
            rule("Review", &[]),
            RuleEntry {
                when: Some({
                    let mut w = WhenClause::default();
                    w.status = Some(ScalarMatcher::In(vec!["Triage".into(), "Backlog".into()]));
                    w
                }),
                state_machine: dummy_sm(),
            },
            RuleEntry {
                when: Some(not_when),
                state_machine: dummy_sm(),
            },
        ];
        let seed = status_seed(&entries, &[]);
        assert!(seed.contains("InProgress"));
        assert!(seed.contains("Review"));
        assert!(seed.contains("Triage"));
        assert!(seed.contains("Backlog"));
        assert!(!seed.contains("Done"));
    }
}
