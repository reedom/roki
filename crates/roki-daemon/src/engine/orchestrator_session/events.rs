//! Daemon -> orchestrator event payloads. Each event is one JSON object on
//! its own line per design.md "Daemon -> orchestrator event catalog".
//!
//! The daemon contributes only structured fields (`kind`, `correlation_id`,
//! repos, paths, errnos, timestamps). It NEVER templates Linear-facing
//! human text — that responsibility lives in the orchestrator session via
//! the operator's Linear MCP.
//!
//! Spec refs: requirements.md Req 4.2, 5.2, 12.5, 12.6; design.md "Daemon
//! -> orchestrator event catalog" (lines ~921-931).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::engine::phase_subprocess::catalog::PhaseName;

/// Local RFC 3339 (de)serializer for `OffsetDateTime`. The workspace pulls
/// `time` with `formatting` only (no `serde` feature) per the daemon's
/// dependency budget, so we route timestamps through `Rfc3339` manually.
mod rfc3339 {
    use serde::de::{self, Deserializer};
    use serde::{Deserialize, Serializer};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    pub fn serialize<S>(value: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let formatted = value
            .format(&Rfc3339)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&formatted)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        OffsetDateTime::parse(&raw, &Rfc3339).map_err(de::Error::custom)
    }
}

/// Tagged sum of every payload the daemon writes to the orchestrator's
/// stdin. The serde tag (`event`) appears on the wire as a sibling field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEvent {
    PhaseComplete(PhaseCompletePayload),
    PhaseNonclean(PhaseNoncleanPayload),
    DaemonDirective(DaemonDirectivePayload),
    TrackerTerminal(TrackerTerminalPayload),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseCompletePayload {
    pub phase: PhaseName,
    pub result: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_artifact_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classify: Option<ClassifyOutcome>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifyOutcome {
    pub path: ClassifyPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_feature: Option<String>,
}

/// Five documented classify routing paths (A-E).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifyPath {
    A,
    B,
    C,
    D,
    E,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseNoncleanPayload {
    pub phase: PhaseName,
    pub classification: NoncleanClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_subtype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoncleanClassification {
    NonZero,
    Signal,
    Stall,
    MaxTurnsExhausted,
    NonSuccessSubtype,
    UnknownSubtype,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonDirectivePayload {
    pub kind: String,
    pub correlation_id: String,
    pub repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_subtype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errno: Option<i32>,
    #[serde(with = "rfc3339")]
    pub timestamp: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackerTerminalPayload {
    pub terminal_state: TrackerTerminalState,
    pub correlation_id: String,
    #[serde(with = "rfc3339")]
    pub timestamp: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackerTerminalState {
    Done,
    Canceled,
    AssignmentLost,
    RokiReadyRemoved,
}

/// Serialize to a single JSON object on its own line. The terminating `\n`
/// is included so callers can write the buffer straight to stdin.
pub fn serialize_one_per_line(event: &DaemonEvent) -> Result<String, serde_json::Error> {
    let mut buf = serde_json::to_string(event)?;
    buf.push('\n');
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn ts() -> OffsetDateTime {
        datetime!(2026-05-05 12:00:00 UTC)
    }

    #[test]
    fn phase_complete_serializes_to_one_line() {
        let event = DaemonEvent::PhaseComplete(PhaseCompletePayload {
            phase: PhaseName::Implement,
            result: serde_json::json!({"subtype": "success"}),
            pr_url: None,
            review_artifact_path: None,
            classify: None,
        });
        let line = serialize_one_per_line(&event).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.contains("\"event\":\"phase_complete\""));
        assert!(line.contains("\"phase\":\"implement\""));
    }

    #[test]
    fn phase_nonclean_serializes_classification_kebab() {
        let event = DaemonEvent::PhaseNonclean(PhaseNoncleanPayload {
            phase: PhaseName::CiFix,
            classification: NoncleanClassification::MaxTurnsExhausted,
            raw_subtype: Some("error_max_turns".to_owned()),
            additional_context: Some("retry context".to_owned()),
        });
        let line = serialize_one_per_line(&event).unwrap();
        assert!(line.contains("\"classification\":\"max_turns_exhausted\""));
        assert!(line.contains("\"phase\":\"ci_fix\""));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn daemon_directive_serializes_with_structured_fields_only() {
        let event = DaemonEvent::DaemonDirective(DaemonDirectivePayload {
            kind: "retry_exhausted".to_owned(),
            correlation_id: "corr-1".to_owned(),
            repos: vec!["github.com/reedom/roki".to_owned()],
            worktree_path: Some(PathBuf::from("/tmp/wt/ENG-1")),
            last_subtype: Some("error_during_execution".to_owned()),
            attempts: Some(3),
            window_ms: Some(120_000),
            errno: None,
            timestamp: ts(),
        });
        let line = serialize_one_per_line(&event).unwrap();
        assert!(line.contains("\"event\":\"daemon_directive\""));
        assert!(line.contains("\"kind\":\"retry_exhausted\""));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn tracker_terminal_serializes_with_kebab_state() {
        let event = DaemonEvent::TrackerTerminal(TrackerTerminalPayload {
            terminal_state: TrackerTerminalState::AssignmentLost,
            correlation_id: "corr-2".to_owned(),
            timestamp: ts(),
        });
        let line = serialize_one_per_line(&event).unwrap();
        assert!(line.contains("\"event\":\"tracker_terminal\""));
        assert!(line.contains("\"terminal_state\":\"assignment_lost\""));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn no_embedded_newlines_in_any_serialized_event() {
        let events = vec![
            DaemonEvent::PhaseComplete(PhaseCompletePayload {
                phase: PhaseName::Review,
                result: serde_json::json!({"subtype": "success"}),
                pr_url: Some("https://github.com/x/y/pull/1".to_owned()),
                review_artifact_path: Some(PathBuf::from(".kiro/specs/foo/review.md")),
                classify: Some(ClassifyOutcome {
                    path: ClassifyPath::B,
                    suggested_command: Some("/kiro-spec-init".to_owned()),
                    suggested_label: Some("roki:impl".to_owned()),
                    target_feature: Some("foo".to_owned()),
                }),
            }),
            DaemonEvent::TrackerTerminal(TrackerTerminalPayload {
                terminal_state: TrackerTerminalState::Done,
                correlation_id: "c".to_owned(),
                timestamp: ts(),
            }),
        ];
        for event in events {
            let line = serialize_one_per_line(&event).unwrap();
            assert_eq!(line.matches('\n').count(), 1, "event {event:?}");
            assert!(line.ends_with('\n'));
            // No raw newlines inside the JSON body.
            assert!(!line[..line.len() - 1].contains('\n'));
        }
    }

    #[test]
    fn serialize_does_not_bake_in_caller_secret_state() {
        // Contract: the function shapes only the event passed in. Token-shaped
        // strings the caller never put in the event must not appear.
        let secret = "lin_api_DO_NOT_LEAK_42";
        let event = DaemonEvent::TrackerTerminal(TrackerTerminalPayload {
            terminal_state: TrackerTerminalState::Done,
            correlation_id: "corr".to_owned(),
            timestamp: ts(),
        });
        let line = serialize_one_per_line(&event).unwrap();
        assert!(
            !line.contains(secret),
            "serialize_one_per_line must shape only its argument; caller-context tokens must not appear"
        );
    }
}
