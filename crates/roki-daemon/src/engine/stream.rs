//! Stream-JSON line tooling.
//!
//! `LineSplitter` consumes async byte streams and yields complete lines.
//! `scan_run_terminal_line` checks whether a parsed line is the claude/codex
//! stream-json `result` event.

#![allow(dead_code)]

use std::pin::Pin;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader, Lines};

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
