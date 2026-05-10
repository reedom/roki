//! `on_failure:` first-match evaluation.
//!
//! Slice 8: `on_failure` entries are canonical `RuleEntry` rows whose
//! `WhenClause` carries `kind` (failure kind, scalar/in/not) and an optional
//! `phase` (renamed semantically to "state id" — same field, broader
//! semantics per spec §11.6). Match is on `(meta.kind, meta.state_id)`.
//!
//! On a match the daemon spawns a `kind: failure` cycle that runs the
//! handler's state machine.

#![allow(dead_code)]

use crate::engine::cycle_state::FailureMetadata;
#[cfg(test)]
use crate::engine::outcome::FailureKind;
use crate::workflow::canonical::{RuleEntry, ScalarMatcher, WhenClause};

/// Walk `entries` in declared order; return the first whose `when:` matcher
/// accepts `meta`. Entries without a `kind:` matcher are skipped — the spec
/// requires every `on_failure` entry to declare a kind matcher.
pub fn route<'a>(entries: &'a [RuleEntry], meta: &FailureMetadata) -> Option<&'a RuleEntry> {
    entries.iter().find(|e| matches_meta(e.when.as_ref(), meta))
}

fn matches_meta(when: Option<&WhenClause>, meta: &FailureMetadata) -> bool {
    let Some(w) = when else {
        return false;
    };
    let Some(kind_matcher) = &w.kind else {
        return false;
    };
    if !scalar_matches(kind_matcher, meta.kind.as_str()) {
        return false;
    }
    // `when.phase` is the legacy operator-facing key; semantics in slice 8
    // is "match the state id".
    if let Some(state_matcher) = &w.phase {
        if !scalar_matches(state_matcher, &meta.state_id) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::canonical::test_helpers as h;
    use crate::workflow::canonical::{StateMachine, Terminal};
    use std::collections::BTreeMap;

    fn meta(kind: FailureKind, state_id: &str) -> FailureMetadata {
        FailureMetadata {
            kind,
            state_id: state_id.into(),
            visit_n: 1,
            error_text: String::new(),
        }
    }

    fn empty_sm() -> StateMachine {
        let mut sm = h::state_machine();
        sm.start = "__success__".into();
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "noop".into(),
            },
        );
        sm
    }

    fn entry(when: Option<WhenClause>) -> RuleEntry {
        RuleEntry {
            when,
            state_machine: empty_sm(),
        }
    }

    fn when_kind(kind: ScalarMatcher) -> WhenClause {
        WhenClause {
            kind: Some(kind),
            ..WhenClause::default()
        }
    }

    fn when_kind_phase(kind: ScalarMatcher, phase: ScalarMatcher) -> WhenClause {
        WhenClause {
            kind: Some(kind),
            phase: Some(phase),
            ..WhenClause::default()
        }
    }

    #[test]
    fn matcher_eq() {
        let entries = vec![entry(Some(when_kind(ScalarMatcher::Eq("stall".into()))))];
        assert!(route(&entries, &meta(FailureKind::Stall, "run")).is_some());
        assert!(route(&entries, &meta(FailureKind::Unparseable, "post")).is_none());
    }

    #[test]
    fn matcher_in() {
        let entries = vec![entry(Some(when_kind(ScalarMatcher::In(vec![
            "unparseable".into(),
            "schema_drift".into(),
        ]))))];
        assert!(route(&entries, &meta(FailureKind::Unparseable, "post")).is_some());
        assert!(route(&entries, &meta(FailureKind::SchemaDrift, "pre")).is_some());
        assert!(route(&entries, &meta(FailureKind::Stall, "run")).is_none());
    }

    #[test]
    fn matcher_not() {
        let entries = vec![entry(Some(when_kind(ScalarMatcher::Not(
            "recursion_bound".into(),
        ))))];
        assert!(route(&entries, &meta(FailureKind::Stall, "run")).is_some());
        assert!(route(&entries, &meta(FailureKind::RecursionBound, "post")).is_none());
    }

    #[test]
    fn matcher_phase_state_id_optional() {
        let entries = vec![entry(Some(when_kind_phase(
            ScalarMatcher::Eq("stall".into()),
            ScalarMatcher::Eq("run".into()),
        )))];
        assert!(route(&entries, &meta(FailureKind::Stall, "run")).is_some());
        assert!(route(&entries, &meta(FailureKind::Stall, "pre")).is_none());
    }

    #[test]
    fn route_first_match_wins() {
        let entries = vec![
            entry(Some(when_kind_phase(
                ScalarMatcher::Eq("stall".into()),
                ScalarMatcher::Eq("pre".into()),
            ))),
            entry(Some(when_kind(ScalarMatcher::Eq("stall".into())))),
        ];
        let hit = route(&entries, &meta(FailureKind::Stall, "run")).unwrap();
        // Second entry wins because the first's state_id matcher fails.
        assert!(hit.when.as_ref().unwrap().phase.is_none());
    }

    #[test]
    fn route_no_match_returns_none() {
        let entries = vec![entry(Some(when_kind(ScalarMatcher::Eq("stall".into()))))];
        assert!(route(&entries, &meta(FailureKind::Unparseable, "post")).is_none());
    }

    #[test]
    fn entry_without_kind_matcher_never_matches() {
        let entries = vec![entry(Some(WhenClause::default()))];
        assert!(route(&entries, &meta(FailureKind::Stall, "run")).is_none());
    }

    #[test]
    fn entry_without_when_clause_never_matches() {
        let entries = vec![entry(None)];
        assert!(route(&entries, &meta(FailureKind::Stall, "run")).is_none());
    }

    // BTreeMap stays imported (silence unused) by referencing it here.
    #[test]
    fn _imports() {
        let _: BTreeMap<String, Terminal> = BTreeMap::new();
    }
}
