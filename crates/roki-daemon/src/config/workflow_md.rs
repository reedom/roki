//! `workflow/*.md` frontmatter parser.
//!
//! Slice 2 reads three optional YAML fields from the leading `---/---`
//! frontmatter block of a workflow .md file:
//! - `session: "session" | "command"` — sets `PhaseBody::Path::shape`.
//! - `stall_seconds: <int>` — sets `PhaseBody::Path::stall_seconds`.
//! - `cli: "<liquid template>"` — already honored by slice 1 as a CLI
//!   override; slice 2 keeps the same field but reads it via this parser
//!   instead of the ad-hoc scan.
//!
//! Missing frontmatter is **not** an error: the file is treated as
//! `session: "session"` (default) with no `stall_seconds` override and no
//! `cli` override.

#![allow(dead_code)]

use std::path::Path;

use serde::Deserialize;

use crate::engine::outcome::PhaseShape;
use crate::error::WorkflowError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMdHeader {
    pub shape: PhaseShape,
    pub stall_seconds: Option<u32>,
    pub cli: Option<String>,
}

impl Default for WorkflowMdHeader {
    fn default() -> Self {
        Self {
            shape: PhaseShape::Session,
            stall_seconds: None,
            cli: None,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawHeader {
    session: Option<String>,
    stall_seconds: Option<i64>,
    cli: Option<String>,
}

/// Parse the leading `---/---` YAML frontmatter from `body` and return both
/// the header struct and the post-frontmatter body slice. When `body` does
/// not begin with `---\n` (or `---\r\n`), returns the default header and the
/// full body.
pub fn parse_workflow_md_frontmatter<'a>(
    path: &Path,
    body: &'a str,
) -> Result<(WorkflowMdHeader, &'a str), WorkflowError> {
    let Some(rest) = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))
    else {
        return Ok((WorkflowMdHeader::default(), body));
    };

    let Some(end) = find_closing_delimiter(rest) else {
        return Err(WorkflowError::WorkflowMdFrontmatter {
            path: path.to_path_buf(),
            reason: "missing closing '---' delimiter".to_string(),
        });
    };

    let yaml = &rest[..end.start];
    let raw: RawHeader =
        serde_yaml_ng::from_str(yaml).map_err(|err| WorkflowError::WorkflowMdFrontmatter {
            path: path.to_path_buf(),
            reason: format!("yaml parse error: {err}"),
        })?;

    let shape = match raw.session.as_deref() {
        None | Some("session") => PhaseShape::Session,
        Some("command") => PhaseShape::Command,
        Some(other) => {
            return Err(WorkflowError::InvalidSessionField {
                path: path.to_path_buf(),
                value: other.to_string(),
            });
        }
    };

    let stall_seconds = match raw.stall_seconds {
        None => None,
        Some(n) if n >= 1 => Some(n as u32),
        Some(other) => {
            return Err(WorkflowError::InvalidStallSeconds {
                path: path.to_path_buf(),
                value: other.to_string(),
            });
        }
    };

    let header = WorkflowMdHeader {
        shape,
        stall_seconds,
        cli: raw.cli.filter(|s| !s.is_empty()),
    };

    Ok((header, &rest[end.end..]))
}

struct Span {
    start: usize,
    end: usize,
}

fn find_closing_delimiter(rest: &str) -> Option<Span> {
    let mut search_from = 0;
    while let Some(idx) = rest[search_from..].find("---") {
        let abs = search_from + idx;
        let at_line_start = abs == 0 || rest.as_bytes()[abs - 1] == b'\n';
        if !at_line_start {
            search_from = abs + 3;
            continue;
        }
        let after = &rest[abs + 3..];
        if let Some(stripped) = after.strip_prefix('\n') {
            let consumed_end = abs + 3 + (after.len() - stripped.len());
            return Some(Span {
                start: abs,
                end: consumed_end,
            });
        }
        if let Some(stripped) = after.strip_prefix("\r\n") {
            let consumed_end = abs + 3 + (after.len() - stripped.len());
            return Some(Span {
                start: abs,
                end: consumed_end,
            });
        }
        if after.is_empty() {
            return Some(Span {
                start: abs,
                end: rest.len(),
            });
        }
        search_from = abs + 3;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(body: &str) -> Result<(WorkflowMdHeader, String), WorkflowError> {
        let (h, rest) = parse_workflow_md_frontmatter(Path::new("/tmp/x.md"), body)?;
        Ok((h, rest.to_string()))
    }

    #[test]
    fn no_frontmatter_returns_default_header() {
        let (h, body) = run("# Hello\n").unwrap();
        assert_eq!(h.shape, PhaseShape::Session);
        assert!(h.stall_seconds.is_none());
        assert!(h.cli.is_none());
        assert_eq!(body, "# Hello\n");
    }

    #[test]
    fn explicit_session_command() {
        let (h, _) = run("---\nsession: \"command\"\n---\nbody\n").unwrap();
        assert_eq!(h.shape, PhaseShape::Command);
    }

    #[test]
    fn explicit_session_session() {
        let (h, _) = run("---\nsession: \"session\"\n---\nbody\n").unwrap();
        assert_eq!(h.shape, PhaseShape::Session);
    }

    #[test]
    fn invalid_session_value_is_rejected() {
        match run("---\nsession: \"bogus\"\n---\nbody\n") {
            Err(WorkflowError::InvalidSessionField { value, .. }) => {
                assert_eq!(value, "bogus");
            }
            other => panic!("expected InvalidSessionField, got {other:?}"),
        }
    }

    #[test]
    fn stall_seconds_parsed_and_validated() {
        let (h, _) = run("---\nstall_seconds: 42\n---\nbody\n").unwrap();
        assert_eq!(h.stall_seconds, Some(42));
        match run("---\nstall_seconds: 0\n---\nbody\n") {
            Err(WorkflowError::InvalidStallSeconds { value, .. }) => {
                assert_eq!(value, "0");
            }
            other => panic!("expected InvalidStallSeconds, got {other:?}"),
        }
    }

    #[test]
    fn cli_override_picked_up() {
        let (h, _) = run("---\ncli: \"claude --print\"\n---\nbody\n").unwrap();
        assert_eq!(h.cli.as_deref(), Some("claude --print"));
    }

    #[test]
    fn missing_closing_delimiter_is_error() {
        match run("---\nsession: \"session\"\nbody without closing\n") {
            Err(WorkflowError::WorkflowMdFrontmatter { reason, .. }) => {
                assert!(reason.contains("missing closing"));
            }
            other => panic!("expected WorkflowMdFrontmatter, got {other:?}"),
        }
    }

    #[test]
    fn body_after_frontmatter_returned_verbatim() {
        let (_, body) = run("---\nsession: \"session\"\n---\n# title\n\nparagraph\n").unwrap();
        assert_eq!(body, "# title\n\nparagraph\n");
    }
}
