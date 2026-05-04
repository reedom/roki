//! Orchestrator response schema types AND the per-turn stdout extractor.
//!
//! The schema surface (types + JSON-Schema) is exposed unchanged for
//! cross-module consumers; the [`ActionParser`] state machine drains a
//! turn's stdout lines and emits exactly one [`ParseTurnOutcome`] per turn,
//! tracking consecutive schema-drift turns so the adapter can issue the
//! documented one-shot reprompt before terminal drift.
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

/// Per-turn extraction outcome.
///
/// On schema drift the adapter issues exactly one reprompt with a schema
/// reminder body; a second consecutive drift terminates the orchestrator
/// session via `Inactive(orchestrator_unparseable)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseTurnOutcome {
    /// Final parseable JSON object validated as a typed action.
    Action(OrchestratorAction),
    /// First-time drift; caller issues a one-shot reprompt with the schema
    /// reminder body.
    Drift { reprompt_payload: String },
    /// Second consecutive drift; caller routes to
    /// `Inactive(orchestrator_unparseable)` and surfaces the raw last line
    /// for operator diagnosis.
    TerminalDrift { last_raw_stdout: String },
}

/// Stateful per-session parser. The state is just the consecutive-drift
/// counter; a successful action parse resets it.
#[derive(Debug, Default)]
pub struct ActionParser {
    drift_count: u32,
}

impl ActionParser {
    pub fn new() -> Self {
        Self { drift_count: 0 }
    }

    /// Current consecutive drift count (0 after each successful parse).
    pub fn drift_count(&self) -> u32 {
        self.drift_count
    }

    /// Drain a complete turn's stdout (already line-split). Earlier
    /// emissions are advisory-only; the LAST parseable JSON object is the
    /// authoritative action. Extended-thinking lines (`{"type":"thinking"`
    /// envelope or a `<thinking>...</thinking>` literal) are skipped before
    /// the last-object scan.
    pub fn parse_turn(&mut self, lines: &[String]) -> ParseTurnOutcome {
        let candidate_lines: Vec<&String> = lines
            .iter()
            .filter(|line| !is_thinking_line(line))
            .collect();

        let last_json_object = candidate_lines
            .iter()
            .rev()
            .find_map(|line| try_parse_json_object(line));

        let Some(value) = last_json_object else {
            return self.record_drift(last_raw(lines));
        };

        match serde_json::from_value::<OrchestratorAction>(value) {
            Ok(action) => match validate_action(&action) {
                Ok(()) => {
                    self.drift_count = 0;
                    ParseTurnOutcome::Action(action)
                }
                Err(_) => self.record_drift(last_raw(lines)),
            },
            Err(_) => self.record_drift(last_raw(lines)),
        }
    }

    fn record_drift(&mut self, last_raw_stdout: String) -> ParseTurnOutcome {
        self.drift_count += 1;
        if self.drift_count >= 2 {
            // Reset so a recovery does not start at 1; orchestrator will be
            // torn down before this parser is reused, but keep the invariant
            // sane for any caller that holds the handle past terminal drift.
            ParseTurnOutcome::TerminalDrift { last_raw_stdout }
        } else {
            ParseTurnOutcome::Drift {
                reprompt_payload: schema_reminder_payload(),
            }
        }
    }
}

/// Body of the one-shot drift reprompt: a stable schema reminder. The
/// orchestrator-session adapter writes it on the orchestrator's stdin.
fn schema_reminder_payload() -> String {
    let schema = &*ORCHESTRATOR_ACTION_JSON_SCHEMA;
    format!(
        "Your previous turn did not produce a parseable orchestrator \
         action. Reply with EXACTLY one JSON object on its own line that \
         conforms to the following schema and nothing else.\n\n{schema}"
    )
}

fn last_raw(lines: &[String]) -> String {
    lines
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .cloned()
        .unwrap_or_default()
}

fn is_thinking_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("<thinking>") || trimmed.starts_with("</thinking>") {
        return true;
    }
    // `{"type":"thinking"...` envelope (allow optional whitespace inside).
    if let Some(rest) = trimmed.strip_prefix('{') {
        let mut compact = rest.replace([' ', '\t'], "");
        compact.insert(0, '{');
        if compact.starts_with("{\"type\":\"thinking\"") {
            return true;
        }
    }
    false
}

fn try_parse_json_object(line: &str) -> Option<serde_json::Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if value.is_object() { Some(value) } else { None }
}

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

    fn line(s: &str) -> String {
        s.to_owned()
    }

    fn run_phase_action_json(phase: &str) -> String {
        format!(
            r#"{{"action":"run_phase","phase":"{phase}","reason":"go {phase}"}}"#
        )
    }

    #[test]
    fn parse_turn_extracts_only_the_final_json_object() {
        let mut parser = ActionParser::new();
        // Advisory progress emissions before the final action.
        let lines = vec![
            line(r#"{"type":"system","subtype":"progress","note":"thinking"}"#),
            line("plain prose advisory line"),
            line(&run_phase_action_json("implement")),
        ];
        match parser.parse_turn(&lines) {
            ParseTurnOutcome::Action(action) => {
                assert_eq!(action.action, ActionKind::RunPhase);
                assert_eq!(action.phase, Some(PhaseName::Implement));
                assert_eq!(action.reason.as_str(), "go implement");
            }
            other => panic!("expected Action, got {other:?}"),
        }
        assert_eq!(parser.drift_count(), 0);
    }

    #[test]
    fn parse_turn_skips_extended_thinking_lines() {
        let mut parser = ActionParser::new();
        let lines = vec![
            line(r#"{"type":"thinking","content":"weighing options"}"#),
            line("<thinking>scratch</thinking>"),
            line(&run_phase_action_json("review")),
        ];
        match parser.parse_turn(&lines) {
            ParseTurnOutcome::Action(action) => {
                assert_eq!(action.phase, Some(PhaseName::Review));
            }
            other => panic!("expected Action, got {other:?}"),
        }
    }

    #[test]
    fn first_drift_emits_reprompt_second_emits_terminal_drift() {
        let mut parser = ActionParser::new();
        let drift_lines = vec![line("oops, no JSON here")];

        match parser.parse_turn(&drift_lines) {
            ParseTurnOutcome::Drift { reprompt_payload } => {
                assert!(reprompt_payload.contains("OrchestratorAction"));
            }
            other => panic!("expected Drift, got {other:?}"),
        }
        assert_eq!(parser.drift_count(), 1);

        let second_drift = vec![
            line("still no parseable object"),
            line("just prose"),
        ];
        match parser.parse_turn(&second_drift) {
            ParseTurnOutcome::TerminalDrift { last_raw_stdout } => {
                assert_eq!(last_raw_stdout, "just prose");
            }
            other => panic!("expected TerminalDrift, got {other:?}"),
        }
    }

    #[test]
    fn successful_parse_after_one_drift_resets_counter() {
        let mut parser = ActionParser::new();
        let _ = parser.parse_turn(&[line("garbage")]);
        assert_eq!(parser.drift_count(), 1);

        let recovery = vec![line(&run_phase_action_json("classify"))];
        match parser.parse_turn(&recovery) {
            ParseTurnOutcome::Action(action) => {
                assert_eq!(action.phase, Some(PhaseName::Classify));
            }
            other => panic!("expected Action, got {other:?}"),
        }
        assert_eq!(parser.drift_count(), 0);

        // A subsequent fresh drift must be treated as a first drift, not as
        // a terminal one — the counter reset above is what guarantees this.
        match parser.parse_turn(&[line("garbage again")]) {
            ParseTurnOutcome::Drift { .. } => {}
            other => panic!("expected fresh Drift, got {other:?}"),
        }
    }

    #[test]
    fn cross_field_validation_failure_treated_as_drift() {
        let mut parser = ActionParser::new();
        // run_phase requires phase; the wire shape parses but
        // validate_action rejects it.
        let invalid =
            line(r#"{"action":"run_phase","reason":"missing phase"}"#);
        match parser.parse_turn(&[invalid]) {
            ParseTurnOutcome::Drift { .. } => {}
            other => panic!("expected Drift on cross-field invalid, got {other:?}"),
        }
    }
}
