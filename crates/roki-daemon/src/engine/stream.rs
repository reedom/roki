//! Tolerant newline-delimited stream-json parser for Claude Code worker output.
//!
//! Task 2.7 of the roki-mvp spec. Maps documented stream-json line shapes to
//! the typed [`EngineLifecycleEvent`] taxonomy defined in design.md and
//! intentionally absorbs single-line parse errors so that a malformed line
//! cannot abort the worker event stream (Requirement 5.2).
//!
//! The parser is intentionally a *pure function* over a single line. The
//! supervisor (task 2.10) drives a `BufReader::lines()` over the subprocess'
//! stdout and translates each yielded line through [`parse_line`] before
//! feeding the result into the per-issue state machine.
//!
//! Design constraints honored here:
//!
//! * Keyed on the stable `type` field; unknown values map to
//!   [`EngineLifecycleEvent::AgentMessage`] so the supervisor loop continues
//!   to record progress timestamps when Claude Code adds new event shapes
//!   ("schema drift" mitigation, design.md §Engine).
//! * One bad JSON line yields exactly one [`EngineLifecycleEvent::Error`]
//!   plus exactly one `tracing::warn!` event; subsequent lines parse
//!   independently.
//! * Empty / whitespace-only lines are silently skipped (no event, no log).

use serde::Deserialize;

/// Subset of the [`crate::engine`] lifecycle event taxonomy defined in
/// design.md §Engine.
///
/// The terminal `Exited(WorkerOutcome)` variant from the design is emitted by
/// the subprocess supervisor (task 2.10) when the OS process exits, *not* by
/// the line parser, so it is intentionally absent here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum EngineLifecycleEvent {
    /// Subprocess session bootstrap. Emitted for the documented
    /// `{"type":"system","subtype":"init",...}` line at the start of a
    /// `claude --print --output-format stream-json` session.
    Started,
    /// Generic non-empty event. Used for `assistant` / `user` text envelopes
    /// and — critically — as the catch-all bucket for unknown `type` values
    /// to keep the supervisor loop's progress timestamps advancing across
    /// stream-json schema drift.
    AgentMessage,
    /// Agent invoked a registered tool. `name` is the tool identifier as
    /// reported in the stream-json `tool_use` content block.
    ToolCall { name: String },
    /// Tool invocation completed. `ok` reports whether the tool reported a
    /// non-error result.
    ToolResult { name: String, ok: bool },
    /// Either the line could not be parsed as JSON, or the line carried a
    /// `result`-shaped payload that signalled an error.
    Error { message: String },
}

/// Parse a single newline-delimited stream-json line into an
/// [`EngineLifecycleEvent`].
///
/// Returns `None` only for empty / whitespace-only lines. A malformed JSON
/// line returns `Some(EngineLifecycleEvent::Error { ... })` and emits a
/// single `tracing::warn!` event so the operator can correlate the parse
/// error with the originating issue.
pub fn parse_line(line: &str) -> Option<EngineLifecycleEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            // Tolerant-by-design: the worker stream must not abort on a
            // single bad line. Surface the failure to both the structured
            // log stream and the typed event stream.
            let message = format!("invalid stream-json line: {error}");
            tracing::warn!(
                target: "engine.stream",
                error = %error,
                line = %trimmed,
                "stream-json parse error"
            );
            return Some(EngineLifecycleEvent::Error { message });
        }
    };

    Some(map_value(&value))
}

fn map_value(value: &serde_json::Value) -> EngineLifecycleEvent {
    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "system" => map_system(value),
        "assistant" | "user" => map_message(value),
        "result" => map_result(value),
        // Unknown / future event shapes are intentionally treated as
        // AgentMessage so the supervisor loop's progress timestamps continue
        // to advance across Claude Code stream-json schema additions.
        _ => EngineLifecycleEvent::AgentMessage,
    }
}

fn map_system(value: &serde_json::Value) -> EngineLifecycleEvent {
    match value.get("subtype").and_then(|v| v.as_str()) {
        Some("init") => EngineLifecycleEvent::Started,
        // Other system subtypes (e.g. compact_boundary) are progress signals;
        // route them to AgentMessage so they keep the stall timer alive.
        _ => EngineLifecycleEvent::AgentMessage,
    }
}

fn map_message(value: &serde_json::Value) -> EngineLifecycleEvent {
    // The `assistant` / `user` envelopes wrap a Claude API message whose
    // `content` array can hold mixed text + tool_use + tool_result blocks.
    // Project the first tool-related block onto a typed event when present;
    // otherwise treat the envelope as a generic agent message.
    let blocks = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array());

    if let Some(blocks) = blocks {
        for block in blocks {
            if let Some(event) = ContentBlock::project(block) {
                return event;
            }
        }
    }

    EngineLifecycleEvent::AgentMessage
}

fn map_result(value: &serde_json::Value) -> EngineLifecycleEvent {
    // `claude --print` emits a single `result` envelope at session end.
    // When `is_error` is true the supervisor needs a typed Error event so
    // the orchestrator can classify the worker outcome; otherwise the
    // envelope is a progress event and maps to AgentMessage.
    let is_error = value
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !is_error {
        return EngineLifecycleEvent::AgentMessage;
    }

    let message = value
        .get("result")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("subtype")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "claude reported an error result".to_owned());

    EngineLifecycleEvent::Error { message }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "tool_use")]
    ToolUse { name: String },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: Option<String>,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(other)]
    Other,
}

impl ContentBlock {
    fn project(raw: &serde_json::Value) -> Option<EngineLifecycleEvent> {
        let parsed: ContentBlock = serde_json::from_value(raw.clone()).ok()?;
        match parsed {
            ContentBlock::ToolUse { name } => Some(EngineLifecycleEvent::ToolCall { name }),
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                name,
            } => {
                // The Anthropic message API does not echo the tool name on
                // tool_result blocks; fall back to the tool_use_id so the
                // event still carries a stable correlation key for logs.
                let identifier = name
                    .or(tool_use_id)
                    .unwrap_or_else(|| "<unknown>".to_owned());
                Some(EngineLifecycleEvent::ToolResult {
                    name: identifier,
                    ok: !is_error,
                })
            }
            ContentBlock::Other => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_test::traced_test;

    /// Drive the parser the way the supervisor will: line by line, collecting
    /// every emitted event. Empty / whitespace lines are filtered out.
    fn parse_stream(input: &str) -> Vec<EngineLifecycleEvent> {
        input.lines().filter_map(parse_line).collect()
    }

    #[test]
    #[traced_test]
    fn recorded_stream_with_one_bad_line_emits_all_valid_events_in_order() {
        // Recorded stream-json fixture: a representative session with five
        // valid events plus one malformed line wedged between the tool_use
        // and tool_result events.
        let input = r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude-sonnet-4-5","tools":[]}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Looking at the repo..."}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls"}}]}}
{this is not valid json
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","is_error":false,"content":"src\nREADME.md"}]}}
{"type":"result","subtype":"success","duration_ms":1234,"is_error":false,"result":"Listed the repo."}
"#;

        let events = parse_stream(input);

        assert_eq!(
            events.len(),
            6,
            "expected 5 valid events plus 1 parse-error event, got {events:?}"
        );

        // Valid events appear in the order the worker emitted them.
        assert!(matches!(events[0], EngineLifecycleEvent::Started));
        assert!(matches!(events[1], EngineLifecycleEvent::AgentMessage));
        match &events[2] {
            EngineLifecycleEvent::ToolCall { name } => assert_eq!(name, "Bash"),
            other => panic!("expected ToolCall, got {other:?}"),
        }
        // The bad line lands here and does NOT abort the stream.
        match &events[3] {
            EngineLifecycleEvent::Error { message } => {
                assert!(
                    message.contains("invalid stream-json line"),
                    "error message should describe the parse failure: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        match &events[4] {
            EngineLifecycleEvent::ToolResult { name, ok } => {
                assert_eq!(name, "toolu_01");
                assert!(*ok);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert!(matches!(events[5], EngineLifecycleEvent::AgentMessage));

        // The bad line emitted exactly one warn-level log on the
        // `engine.stream` target. `tracing-test` captures every log line
        // produced during the test.
        assert!(
            logs_contain("stream-json parse error"),
            "expected parse-error warn log to be emitted"
        );
    }

    #[test]
    fn unknown_event_type_maps_to_agent_message() {
        // Forward-compat: a hypothetical future stream-json shape must not
        // crash the supervisor; it must still tick the stall timer.
        let event = parse_line(r#"{"type":"future_thing","payload":{"foo":"bar"}}"#);
        assert_eq!(event, Some(EngineLifecycleEvent::AgentMessage));
    }

    #[test]
    fn missing_type_field_maps_to_agent_message() {
        // Defensive: a JSON object without a `type` field is still valid
        // JSON; treat it as a generic progress signal rather than an error.
        let event = parse_line(r#"{"payload":42}"#);
        assert_eq!(event, Some(EngineLifecycleEvent::AgentMessage));
    }

    #[test]
    fn empty_and_whitespace_lines_are_silently_skipped() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   "), None);
        assert_eq!(parse_line("\t\t  "), None);
    }

    #[test]
    fn multiple_bad_lines_each_produce_their_own_error_event() {
        let input = "not json at all\n{\"type\":\"system\",\"subtype\":\"init\"}\nalso not json\n{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n";
        let events = parse_stream(input);

        assert_eq!(events.len(), 4, "got {events:?}");
        assert!(matches!(events[0], EngineLifecycleEvent::Error { .. }));
        assert!(matches!(events[1], EngineLifecycleEvent::Started));
        assert!(matches!(events[2], EngineLifecycleEvent::Error { .. }));
        assert!(matches!(events[3], EngineLifecycleEvent::AgentMessage));
    }

    #[test]
    fn tool_result_propagates_is_error_flag() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_42","is_error":true,"content":"boom"}]}}"#;
        match parse_line(line) {
            Some(EngineLifecycleEvent::ToolResult { name, ok }) => {
                assert_eq!(name, "toolu_42");
                assert!(!ok);
            }
            other => panic!("expected ToolResult with ok=false, got {other:?}"),
        }
    }

    #[test]
    fn result_envelope_with_is_error_true_maps_to_error() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"max turns reached"}"#;
        match parse_line(line) {
            Some(EngineLifecycleEvent::Error { message }) => {
                assert_eq!(message, "max turns reached");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn system_subtype_other_than_init_is_agent_message() {
        // System subtypes such as `compact_boundary` are progress signals,
        // not session-start signals.
        let line = r#"{"type":"system","subtype":"compact_boundary"}"#;
        assert_eq!(parse_line(line), Some(EngineLifecycleEvent::AgentMessage));
    }
}
