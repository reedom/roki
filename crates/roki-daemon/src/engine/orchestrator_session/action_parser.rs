//! Orchestrator response schema types. The actual stdout extractor lives in
//! task 6.3; this file declares the schema surface so other modules can
//! depend on the types now.
//!
//! Schema reference: design.md "Orchestrator response schema" (lines
//! ~899-919). Field rules:
//!   - `action` always required.
//!   - `phase` required when `action=run_phase`.
//!   - `outcome` required when `action=stop`.
//!   - `reason` bounded to 200 chars.
//!   - `linear_writes` element grammar: `label:<name>` / `comment_posted:<id>`;
//!     other prefixes round-tripped opaquely as `Other`.

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

pub use crate::engine::phase_subprocess::catalog::PhaseName;

/// Maximum length for the `reason` field (200 chars per design.md).
pub const REASON_MAX_LEN: usize = 200;

/// Top-level action verb the orchestrator emits per turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    RunPhase,
    LinearUpdateDone,
    Stop,
}

/// Terminal outcome surfaced when `action=Stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
    Cancelled,
    NeedsOperator,
    SpecIncomplete,
    NeedsSplit,
    AllowlistRejected,
}

/// Linear write acknowledgement element. Grammar:
/// - `label:<name>` -> `Label`
/// - `comment_posted:<id>` -> `CommentPosted`
/// - any other `<prefix>:<value>` -> `Other` (round-tripped verbatim for
///   additive forward-compatibility per design.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinearWriteAck {
    Label(String),
    CommentPosted(String),
    Other(String),
}

impl LinearWriteAck {
    /// Serialize back to the `<prefix>:<value>` wire form.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Label(name) => format!("label:{name}"),
            Self::CommentPosted(id) => format!("comment_posted:{id}"),
            Self::Other(raw) => raw.clone(),
        }
    }

    /// Parse a wire-form string into the typed enum.
    pub fn from_wire(raw: &str) -> Self {
        if let Some(name) = raw.strip_prefix("label:") {
            Self::Label(name.to_owned())
        } else if let Some(id) = raw.strip_prefix("comment_posted:") {
            Self::CommentPosted(id.to_owned())
        } else {
            Self::Other(raw.to_owned())
        }
    }
}

impl Serialize for LinearWriteAck {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_wire())
    }
}

impl<'de> Deserialize<'de> for LinearWriteAck {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&raw))
    }
}

/// 200-char-bounded string newtype enforced at construction time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedString200(String);

impl BoundedString200 {
    pub fn new(value: impl Into<String>) -> Result<Self, SchemaError> {
        let value = value.into();
        if value.chars().count() > REASON_MAX_LEN {
            return Err(SchemaError::ReasonTooLong {
                actual: value.chars().count(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for BoundedString200 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BoundedString200 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(de::Error::custom)
    }
}

/// One parsed orchestrator turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorAction {
    pub action: ActionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<PhaseName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear_writes: Option<Vec<LinearWriteAck>>,
    pub reason: BoundedString200,
}

/// Schema validation failure modes. Caller-side construction checks land
/// here so the parser layer (task 6.3) can surface them through one error
/// type.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SchemaError {
    #[error("`phase` field is required when action=run_phase")]
    MissingPhaseOnRunPhase,

    #[error("`outcome` field is required when action=stop")]
    MissingOutcomeOnStop,

    #[error("`reason` is bounded to 200 chars (got {actual})")]
    ReasonTooLong { actual: usize },
}

/// Validate cross-field invariants that serde alone cannot express.
pub fn validate_action(action: &OrchestratorAction) -> Result<(), SchemaError> {
    match action.action {
        ActionKind::RunPhase if action.phase.is_none() => {
            Err(SchemaError::MissingPhaseOnRunPhase)
        }
        ActionKind::Stop if action.outcome.is_none() => Err(SchemaError::MissingOutcomeOnStop),
        _ => Ok(()),
    }
}

/// Lazily-built JSON-Schema string for the orchestrator response envelope.
/// Consumers in later tasks (parser, prompt rendering) read from this
/// constant so the schema lives in exactly one place.
pub fn orchestrator_action_json_schema() -> String {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "OrchestratorAction",
        "type": "object",
        "required": ["action", "reason"],
        "properties": {
            "action": {
                "type": "string",
                "enum": ["run_phase", "linear_update_done", "stop"]
            },
            "phase": {
                "type": ["string", "null"],
                "enum": [
                    "classify", "implement", "review", "validate",
                    "open_pr", "ci_fix", "finalize_review", null
                ]
            },
            "additional_context": { "type": ["string", "null"] },
            "outcome": {
                "type": ["string", "null"],
                "enum": [
                    "success", "failure", "cancelled", "needs_operator",
                    "spec_incomplete", "needs_split", "allowlist_rejected", null
                ]
            },
            "linear_writes": {
                "type": ["array", "null"],
                "items": { "type": "string" }
            },
            "reason": {
                "type": "string",
                "maxLength": 200
            }
        },
        "allOf": [
            {
                "if": { "properties": { "action": { "const": "run_phase" } } },
                "then": { "required": ["phase"] }
            },
            {
                "if": { "properties": { "action": { "const": "stop" } } },
                "then": { "required": ["outcome"] }
            }
        ]
    })
    .to_string()
}

/// Materialized schema string. Built on first access; the struct is
/// trivially serializable so cost is negligible.
pub static ORCHESTRATOR_ACTION_JSON_SCHEMA: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(orchestrator_action_json_schema);

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(s: &str) -> BoundedString200 {
        BoundedString200::new(s).expect("reason fits the 200-char bound")
    }

    fn run_phase(phase: PhaseName) -> OrchestratorAction {
        OrchestratorAction {
            action: ActionKind::RunPhase,
            phase: Some(phase),
            additional_context: Some("ctx".to_owned()),
            outcome: None,
            linear_writes: None,
            reason: reason("nominate next phase"),
        }
    }

    fn stop(outcome: Outcome) -> OrchestratorAction {
        OrchestratorAction {
            action: ActionKind::Stop,
            phase: None,
            additional_context: None,
            outcome: Some(outcome),
            linear_writes: None,
            reason: reason("stopping"),
        }
    }

    #[test]
    fn run_phase_with_every_phase_validates() {
        for phase in [
            PhaseName::Classify,
            PhaseName::Implement,
            PhaseName::Review,
            PhaseName::Validate,
            PhaseName::OpenPr,
            PhaseName::CiFix,
            PhaseName::FinalizeReview,
        ] {
            validate_action(&run_phase(phase)).unwrap();
        }
    }

    #[test]
    fn stop_with_every_outcome_validates() {
        for outcome in [
            Outcome::Success,
            Outcome::Failure,
            Outcome::Cancelled,
            Outcome::NeedsOperator,
            Outcome::SpecIncomplete,
            Outcome::NeedsSplit,
            Outcome::AllowlistRejected,
        ] {
            validate_action(&stop(outcome)).unwrap();
        }
    }

    #[test]
    fn linear_update_done_validates_without_phase_or_outcome() {
        let action = OrchestratorAction {
            action: ActionKind::LinearUpdateDone,
            phase: None,
            additional_context: None,
            outcome: None,
            linear_writes: Some(vec![LinearWriteAck::Label("roki:impl".to_owned())]),
            reason: reason("acked"),
        };
        validate_action(&action).unwrap();
    }

    #[test]
    fn run_phase_without_phase_is_rejected() {
        let mut action = run_phase(PhaseName::Implement);
        action.phase = None;
        assert_eq!(
            validate_action(&action),
            Err(SchemaError::MissingPhaseOnRunPhase)
        );
    }

    #[test]
    fn stop_without_outcome_is_rejected() {
        let mut action = stop(Outcome::Success);
        action.outcome = None;
        assert_eq!(
            validate_action(&action),
            Err(SchemaError::MissingOutcomeOnStop)
        );
    }

    #[test]
    fn reason_over_200_chars_is_rejected_at_construction() {
        let too_long = "x".repeat(REASON_MAX_LEN + 1);
        assert_eq!(
            BoundedString200::new(too_long).unwrap_err(),
            SchemaError::ReasonTooLong {
                actual: REASON_MAX_LEN + 1
            }
        );
    }

    #[test]
    fn unknown_enum_value_is_rejected_by_serde() {
        let json = r#"{"action":"explode","reason":"nope"}"#;
        let result: Result<OrchestratorAction, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn linear_write_ack_round_trips_label_and_comment() {
        let label = LinearWriteAck::from_wire("label:roki:impl");
        assert_eq!(label, LinearWriteAck::Label("roki:impl".to_owned()));
        assert_eq!(label.to_wire(), "label:roki:impl");

        let comment = LinearWriteAck::from_wire("comment_posted:abc-123");
        assert_eq!(
            comment,
            LinearWriteAck::CommentPosted("abc-123".to_owned())
        );
        assert_eq!(comment.to_wire(), "comment_posted:abc-123");
    }

    #[test]
    fn linear_write_ack_round_trips_unknown_prefix_via_other() {
        let raw = "future_kind:payload";
        let ack = LinearWriteAck::from_wire(raw);
        assert_eq!(ack, LinearWriteAck::Other(raw.to_owned()));
        assert_eq!(ack.to_wire(), raw);
    }

    #[test]
    fn json_schema_constant_is_non_empty_and_parses() {
        let schema = &*ORCHESTRATOR_ACTION_JSON_SCHEMA;
        assert!(!schema.is_empty());
        let parsed: serde_json::Value =
            serde_json::from_str(schema).expect("schema is valid JSON");
        assert_eq!(parsed["title"], "OrchestratorAction");
    }

    #[test]
    fn orchestrator_action_serializes_with_kebab_action() {
        let action = stop(Outcome::Success);
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"action\":\"stop\""));
        assert!(json.contains("\"outcome\":\"success\""));
    }
}
