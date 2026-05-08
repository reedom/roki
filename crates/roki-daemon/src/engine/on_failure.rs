#![allow(dead_code)]

//! `[[on_failure]]` first-match evaluation against a `FailureMeta`.
//!
//! Per fr:06 §53 + §63, `when.kind` accepts:
//!   - single value: `when.kind = "stall"`
//!   - in-array:     `when.kind.in = ["unparseable", "schema_drift"]`
//!   - not:          `when.kind.not = "iter_exhausted"`
//!
//! plus optional `when.phase = "pre" | "run" | "post"`.
//!
//! Exactly one of the three `when.kind` forms may be set per entry; mixing
//! them is a config-load error (`OnFailureKindMatcherConflict`).

use crate::engine::outcome::{FailureKind, FailureMeta, PhaseBody, PhaseKind};

#[derive(Debug, Clone)]
pub enum KindMatcher {
    Eq(FailureKind),
    In(Vec<FailureKind>),
    Not(FailureKind),
}

#[derive(Debug, Clone)]
pub struct OnFailure {
    pub when_kind: KindMatcher,
    pub when_phase: Option<PhaseKind>,
    pub pre: Option<PhaseBody>,
    pub run: PhaseBody,
    pub post: Option<PhaseBody>,
}

impl OnFailure {
    pub fn matches(&self, meta: &FailureMeta) -> bool {
        let kind_ok = match &self.when_kind {
            KindMatcher::Eq(k) => *k == meta.kind,
            KindMatcher::In(ks) => ks.contains(&meta.kind),
            KindMatcher::Not(k) => *k != meta.kind,
        };
        let phase_ok = self.when_phase.is_none_or(|p| p == meta.phase);
        kind_ok && phase_ok
    }
}

pub fn route<'a>(entries: &'a [OnFailure], meta: &FailureMeta) -> Option<&'a OnFailure> {
    entries.iter().find(|e| e.matches(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::outcome::{FailureKind, PhaseKind};

    fn meta(kind: FailureKind, phase: PhaseKind) -> FailureMeta {
        FailureMeta {
            failed_cycle_id: uuid::Uuid::nil(),
            kind,
            phase,
            iter: 1,
            exit_code: None,
            error_text: String::new(),
        }
    }

    fn entry(when_kind: KindMatcher, when_phase: Option<PhaseKind>) -> OnFailure {
        OnFailure {
            when_kind,
            when_phase,
            pre: None,
            run: PhaseBody::InlineCmd { cmd: "true".into() },
            post: None,
        }
    }

    #[test]
    fn matcher_eq() {
        let e = entry(KindMatcher::Eq(FailureKind::Stall), None);
        assert!(e.matches(&meta(FailureKind::Stall, PhaseKind::Run)));
        assert!(!e.matches(&meta(FailureKind::Unparseable, PhaseKind::Post)));
    }

    #[test]
    fn matcher_in() {
        let e = entry(
            KindMatcher::In(vec![FailureKind::Unparseable, FailureKind::SchemaDrift]),
            None,
        );
        assert!(e.matches(&meta(FailureKind::Unparseable, PhaseKind::Post)));
        assert!(e.matches(&meta(FailureKind::SchemaDrift, PhaseKind::Pre)));
        assert!(!e.matches(&meta(FailureKind::Stall, PhaseKind::Run)));
    }

    #[test]
    fn matcher_not() {
        let e = entry(KindMatcher::Not(FailureKind::IterExhausted), None);
        assert!(e.matches(&meta(FailureKind::Stall, PhaseKind::Run)));
        assert!(!e.matches(&meta(FailureKind::IterExhausted, PhaseKind::Post)));
    }

    #[test]
    fn matcher_phase_optional() {
        let e = entry(KindMatcher::Eq(FailureKind::Stall), Some(PhaseKind::Run));
        assert!(e.matches(&meta(FailureKind::Stall, PhaseKind::Run)));
        assert!(!e.matches(&meta(FailureKind::Stall, PhaseKind::Pre)));
    }

    #[test]
    fn route_first_match_wins() {
        let entries = vec![
            entry(KindMatcher::Eq(FailureKind::Stall), Some(PhaseKind::Pre)),
            entry(KindMatcher::Eq(FailureKind::Stall), None),
        ];
        let m = meta(FailureKind::Stall, PhaseKind::Run);
        let hit = route(&entries, &m).unwrap();
        assert!(hit.when_phase.is_none());
    }

    #[test]
    fn route_no_match_returns_none() {
        let entries = vec![entry(KindMatcher::Eq(FailureKind::Stall), None)];
        let m = meta(FailureKind::Unparseable, PhaseKind::Post);
        assert!(route(&entries, &m).is_none());
    }
}
