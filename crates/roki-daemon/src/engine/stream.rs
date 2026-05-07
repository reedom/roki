//! Stream-JSON line tooling.
//!
//! `LineSplitter` consumes async byte streams and yields complete lines.
//! `scan_directive_line` checks whether a parsed line carries a legal
//! `directive` value for a phase. `scan_run_terminal_line` checks whether
//! a parsed line is the claude/codex stream-json `result` event.

#![allow(dead_code)]

use std::pin::Pin;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader, Lines};

use crate::engine::outcome::{PhaseKind, PostDirective, PreDirective};

/// Lazy line iterator over a tokio async reader. Each call to `next_line`
/// returns one `\n`-terminated line (the trailing newline stripped) or `None`
/// at EOF. Lines may be of arbitrary length; tokio's BufReader does not
/// impose a length cap.
pub struct LineSplitter<R: AsyncRead + Unpin + Send> {
    inner: Lines<BufReader<R>>,
}

impl<R: AsyncRead + Unpin + Send> LineSplitter<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader).lines(),
        }
    }

    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        Pin::new(&mut self.inner).next_line().await
    }
}

/// Result of inspecting a stdout line for a phase directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectiveScan {
    /// Line was not parseable as a JSON object.
    NotJson,
    /// Parseable but does not carry a `directive` field — advisory event.
    Advisory,
    /// Has a `directive` field whose value is outside the legal set.
    SchemaDrift,
    /// Has a legal `directive` for the current phase. The full parsed value
    /// is returned so the caller can write it as the response.
    PreTerminal {
        directive: PreDirective,
        value: Value,
    },
    PostTerminal {
        directive: PostDirective,
        value: Value,
    },
}

/// Inspect a stdout line for a directive. `kind` decides which legal set
/// to validate against (Pre: run/end; Post: pre/run/end).
pub fn scan_directive_line(line: &str, kind: PhaseKind) -> DirectiveScan {
    let value: Value = match serde_json::from_str::<Value>(line) {
        Ok(v) if v.is_object() => v,
        _ => return DirectiveScan::NotJson,
    };

    let Some(directive_str) = value.get("directive").and_then(|v| v.as_str()) else {
        return DirectiveScan::Advisory;
    };

    match kind {
        PhaseKind::Pre => match PreDirective::try_from_str(directive_str) {
            Some(d) => DirectiveScan::PreTerminal { directive: d, value },
            None => DirectiveScan::SchemaDrift,
        },
        PhaseKind::Post => match PostDirective::try_from_str(directive_str) {
            Some(d) => DirectiveScan::PostTerminal { directive: d, value },
            None => DirectiveScan::SchemaDrift,
        },
        PhaseKind::Run => DirectiveScan::Advisory,
    }
}

/// Inspect a stdout line for the claude/codex stream-json `result` event.
/// Returns the parsed value when `type == "result"`, else `None`.
pub fn scan_run_terminal_line(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    if !value.is_object() {
        return None;
    }
    if value.get("type").and_then(|v| v.as_str()) == Some("result") {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn line_splitter_yields_lines_then_none() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        writer.write_all(b"alpha\nbeta\ngamma").await.unwrap();
        drop(writer);
        let mut split = LineSplitter::new(reader);
        assert_eq!(split.next_line().await.unwrap().unwrap(), "alpha");
        assert_eq!(split.next_line().await.unwrap().unwrap(), "beta");
        assert_eq!(split.next_line().await.unwrap().unwrap(), "gamma");
        assert_eq!(split.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn line_splitter_handles_long_line() {
        let (mut writer, reader) = tokio::io::duplex(1 << 17); // 128 KiB
        let mut huge = String::with_capacity(1 << 16);
        huge.extend(std::iter::repeat_n('x', 1 << 16));
        let payload = format!("{huge}\nshort\n");
        writer.write_all(payload.as_bytes()).await.unwrap();
        drop(writer);
        let mut split = LineSplitter::new(reader);
        let big = split.next_line().await.unwrap().unwrap();
        assert_eq!(big.len(), 1 << 16);
        assert_eq!(split.next_line().await.unwrap().unwrap(), "short");
    }

    #[test]
    fn scan_directive_line_pre_terminal() {
        let scan = scan_directive_line(r#"{"directive":"run","payload":{"x":1}}"#, PhaseKind::Pre);
        match scan {
            DirectiveScan::PreTerminal { directive, value } => {
                assert_eq!(directive, PreDirective::Run);
                assert!(value.get("payload").is_some());
            }
            other => panic!("expected PreTerminal, got {other:?}"),
        }
    }

    #[test]
    fn scan_directive_line_post_terminal_end() {
        let scan = scan_directive_line(r#"{"directive":"end"}"#, PhaseKind::Post);
        assert!(matches!(
            scan,
            DirectiveScan::PostTerminal {
                directive: PostDirective::End,
                ..
            }
        ));
    }

    #[test]
    fn scan_directive_line_schema_drift() {
        let scan = scan_directive_line(r#"{"directive":"halt"}"#, PhaseKind::Post);
        assert_eq!(scan, DirectiveScan::SchemaDrift);
    }

    #[test]
    fn scan_directive_line_advisory_when_no_directive_field() {
        let scan = scan_directive_line(r#"{"type":"thinking","text":"…"}"#, PhaseKind::Post);
        assert_eq!(scan, DirectiveScan::Advisory);
    }

    #[test]
    fn scan_directive_line_not_json_for_garbage() {
        assert_eq!(scan_directive_line("not json", PhaseKind::Post), DirectiveScan::NotJson);
        assert_eq!(scan_directive_line("[1,2,3]", PhaseKind::Post), DirectiveScan::NotJson);
        assert_eq!(scan_directive_line("\"plain string\"", PhaseKind::Post), DirectiveScan::NotJson);
    }

    #[test]
    fn scan_directive_line_pre_rejects_pre_value() {
        let scan = scan_directive_line(r#"{"directive":"pre"}"#, PhaseKind::Pre);
        assert_eq!(scan, DirectiveScan::SchemaDrift);
    }

    #[test]
    fn scan_run_terminal_recognises_result_event() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;
        let v = scan_run_terminal_line(line).unwrap();
        assert_eq!(v.get("subtype").and_then(|v| v.as_str()), Some("success"));
    }

    #[test]
    fn scan_run_terminal_ignores_non_result_event() {
        assert!(scan_run_terminal_line(r#"{"type":"thinking"}"#).is_none());
        assert!(scan_run_terminal_line(r#"{"foo":"bar"}"#).is_none());
        assert!(scan_run_terminal_line("not json").is_none());
    }
}
