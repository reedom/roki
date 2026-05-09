//! In-memory escalation queue entry (fr:06 §Escalation queue).
//!
//! Cycle-bound entries (`failure-handler cycle that itself failed`,
//! `cleanup-time fs error`) carry concrete `ticket_id`, `cycle_id`, `phase`.
//! Cycle-less entries (`daemon-internal error with no cycle association`,
//! e.g. cold-start orphan reconcile fs error) leave all three as `None`.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::engine::outcome::{FailureKind, PhaseKind};

#[derive(Debug, Clone)]
pub struct EscalationEntry {
    pub ticket_id: Option<String>,
    pub cycle_id: Option<Uuid>,
    pub failure_kind: FailureKind,
    pub phase: Option<PhaseKind>,
    pub timestamp: OffsetDateTime,
    pub error_text: String,
}

impl EscalationEntry {
    pub fn cycle(
        ticket_id: String,
        cycle_id: Uuid,
        failure_kind: FailureKind,
        phase: PhaseKind,
        error_text: String,
    ) -> Self {
        Self {
            ticket_id: Some(ticket_id),
            cycle_id: Some(cycle_id),
            failure_kind,
            phase: Some(phase),
            timestamp: OffsetDateTime::now_utc(),
            error_text: sanitize(&error_text),
        }
    }

    pub fn daemon(failure_kind: FailureKind, error_text: String) -> Self {
        Self {
            ticket_id: None,
            cycle_id: None,
            failure_kind,
            phase: None,
            timestamp: OffsetDateTime::now_utc(),
            error_text: sanitize(&error_text),
        }
    }
}

/// Strip ASCII control characters except tab and newline; replace invalid
/// UTF-8 with U+FFFD (already enforced by `String`). The HTTP API and TUI
/// apply ANSI strip + HTML escape on read; sanitize here only enforces the
/// invariant that `error_text` does not break the JSONL writer.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\t' || *c == '\n' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi_csi_and_keeps_tabs() {
        let raw = "before\x1b[31mred\x1b[0m\tafter\nline2";
        let s = sanitize(raw);
        assert!(!s.contains('\x1b'), "ANSI ESC must be stripped");
        assert!(s.contains('\t'));
        assert!(s.contains('\n'));
        assert!(s.contains("red"));
    }

    #[test]
    fn cycle_constructor_sets_all_fields() {
        let id = Uuid::new_v4();
        let e = EscalationEntry::cycle(
            "TEAM-1".to_string(),
            id,
            FailureKind::FsPoison,
            PhaseKind::Post,
            "msg".to_string(),
        );
        assert_eq!(e.ticket_id.as_deref(), Some("TEAM-1"));
        assert_eq!(e.cycle_id, Some(id));
        assert_eq!(e.phase, Some(PhaseKind::Post));
        assert_eq!(e.failure_kind, FailureKind::FsPoison);
    }

    #[test]
    fn daemon_constructor_leaves_cycle_fields_none() {
        let e = EscalationEntry::daemon(FailureKind::FsPoison, "boom".to_string());
        assert!(e.ticket_id.is_none());
        assert!(e.cycle_id.is_none());
        assert!(e.phase.is_none());
    }
}
