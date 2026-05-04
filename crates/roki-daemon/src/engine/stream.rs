//! `claude --output-format stream-json` line parser.
//!
//! Two surfaces:
//! 1. [`parse_line`] — newline-delimited JSON line → typed [`StreamLine`]
//!    lifecycle event. Unknown shapes round-trip via [`StreamLine::Other`]
//!    so callers can still observe future fields without forcing a parser
//!    change.
//! 2. [`classify_terminal`] — typed classification of `result.subtype` into
//!    [`TerminalSubtype`] preserving raw strings on unrecognized values.
//!
//! Spec refs: requirements.md Req 5.2, 5.9; design.md "Stream-JSON
//! lifecycle".

use serde_json::Value;
use thiserror::Error;

/// One typed line from `claude --output-format stream-json` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamLine {
    /// `{"type":"system", ...}` — session/system events. `subtype` carries
    /// the documented variant (e.g., `init`).
    System { subtype: String, payload: Value },
    /// `{"type":"assistant", ...}` — assistant message turn.
    Assistant { content: Value },
    /// `{"type":"tool_use", ...}` — tool-use record. `name` is the tool
    /// identifier; `input` is the tool's argument object.
    ToolUse { name: String, input: Value },
    /// `{"type":"result", ...}` — terminal turn. `subtype` is the lifecycle
    /// label fed to [`classify_terminal`].
    Result { subtype: String, payload: Value },
    /// Any other `type` value or shape we have not modeled. Round-tripped
    /// verbatim so the daemon can still log and forward it.
    Other(Value),
}

/// Parser failure modes.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("stream-json line is not valid JSON: {source}")]
    InvalidJson {
        #[source]
        source: serde_json::Error,
    },
    #[error("stream-json line is not a JSON object")]
    NotObject,
}

/// Parse a single newline-delimited stream-json line.
///
/// Empty / whitespace-only input is treated as malformed JSON since stream-
/// json is a strict NDJSON grammar (each transmitted line carries one
/// object). Caller is expected to slice on `\n` and skip empties first.
pub fn parse_line(text: &str) -> Result<StreamLine, ParseError> {
    let value: Value =
        serde_json::from_str(text).map_err(|source| ParseError::InvalidJson { source })?;
    let object = match &value {
        Value::Object(o) => o,
        _ => return Err(ParseError::NotObject),
    };

    let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "system" => Ok(StreamLine::System {
            subtype: object
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            payload: value,
        }),
        "assistant" => Ok(StreamLine::Assistant {
            content: object
                .get("message")
                .cloned()
                .unwrap_or(Value::Null),
        }),
        "tool_use" => Ok(StreamLine::ToolUse {
            name: object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            input: object.get("input").cloned().unwrap_or(Value::Null),
        }),
        "result" => Ok(StreamLine::Result {
            subtype: object
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            payload: value,
        }),
        _ => Ok(StreamLine::Other(value)),
    }
}

/// Documented `result.subtype` lifecycle classifications.
///
/// `success` is the only clean terminal. Other values either map to a known
/// non-success label the orchestrator routes to a remediation strategy
/// ([`Self::ErrorMaxTurns`], [`Self::ErrorDuringExecution`],
/// [`Self::NonSuccessKnown`]) or pass through verbatim
/// ([`Self::Unknown`]) so additive subtypes from upstream do not require a
/// parser change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalSubtype {
    Success,
    ErrorMaxTurns,
    ErrorDuringExecution,
    NonSuccessKnown(String),
    Unknown(String),
}

/// Documented non-success subtypes that are not yet promoted to a typed
/// variant. Anything not in this list and not `success`/`error_*` maps to
/// `Unknown` — preserving the raw string so it can still be forwarded as
/// `raw_subtype` on the `phase_nonclean` event.
const KNOWN_NON_SUCCESS_SUBTYPES: &[&str] = &[
    "error_user_interrupt",
    "error_user_aborted",
    "error_tool_use",
    "error_invalid_input",
];

/// Classify a `result.subtype` string into [`TerminalSubtype`].
pub fn classify_terminal(subtype: &str) -> TerminalSubtype {
    match subtype {
        "success" => TerminalSubtype::Success,
        "error_max_turns" => TerminalSubtype::ErrorMaxTurns,
        "error_during_execution" => TerminalSubtype::ErrorDuringExecution,
        other if KNOWN_NON_SUCCESS_SUBTYPES.contains(&other) => {
            TerminalSubtype::NonSuccessKnown(other.to_owned())
        }
        other => TerminalSubtype::Unknown(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_system_init_line() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        match parse_line(line).unwrap() {
            StreamLine::System { subtype, payload } => {
                assert_eq!(subtype, "init");
                assert_eq!(payload["session_id"], json!("abc"));
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn parse_assistant_line_extracts_message_object() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":"hello"}}"#;
        match parse_line(line).unwrap() {
            StreamLine::Assistant { content } => {
                assert_eq!(content["role"], json!("assistant"));
                assert_eq!(content["content"], json!("hello"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_use_line_extracts_name_and_input() {
        let line = r#"{"type":"tool_use","name":"Bash","input":{"command":"ls"}}"#;
        match parse_line(line).unwrap() {
            StreamLine::ToolUse { name, input } => {
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], json!("ls"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_line_carries_subtype_and_full_payload() {
        let line = r#"{"type":"result","subtype":"success","total_cost":0.1}"#;
        match parse_line(line).unwrap() {
            StreamLine::Result { subtype, payload } => {
                assert_eq!(subtype, "success");
                assert_eq!(payload["total_cost"], json!(0.1));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_type_falls_through_to_other() {
        let line = r#"{"type":"future_kind","extra":42}"#;
        match parse_line(line).unwrap() {
            StreamLine::Other(value) => {
                assert_eq!(value["type"], json!("future_kind"));
                assert_eq!(value["extra"], json!(42));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_json_is_rejected() {
        let result = parse_line("{not json");
        assert!(matches!(result, Err(ParseError::InvalidJson { .. })));
    }

    #[test]
    fn parse_non_object_is_rejected() {
        let result = parse_line("[1,2,3]");
        assert!(matches!(result, Err(ParseError::NotObject)));
    }

    #[test]
    fn classify_terminal_success() {
        assert_eq!(classify_terminal("success"), TerminalSubtype::Success);
    }

    #[test]
    fn classify_terminal_error_max_turns() {
        assert_eq!(
            classify_terminal("error_max_turns"),
            TerminalSubtype::ErrorMaxTurns
        );
    }

    #[test]
    fn classify_terminal_error_during_execution() {
        assert_eq!(
            classify_terminal("error_during_execution"),
            TerminalSubtype::ErrorDuringExecution
        );
    }

    #[test]
    fn classify_terminal_known_non_success_preserves_raw_string() {
        for subtype in KNOWN_NON_SUCCESS_SUBTYPES {
            assert_eq!(
                classify_terminal(subtype),
                TerminalSubtype::NonSuccessKnown((*subtype).to_owned())
            );
        }
    }

    #[test]
    fn classify_terminal_unknown_passes_verbatim() {
        let unknown = "error_future_unknown_signal";
        assert_eq!(
            classify_terminal(unknown),
            TerminalSubtype::Unknown(unknown.to_owned())
        );
    }
}
