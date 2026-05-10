//! Per-state directive-file control channel.
//!
//! Spec: §2.3, §5.1, §5.2, §5.3.
//!
//! The daemon allocates a unique path per state invocation under
//! `<session_tempdir>/directives/<state_id>.<visit_n>.json` and exposes it via
//! `ROKI_DIRECTIVE_PATH`. The subprocess writes its directive (JSON object
//! with required `directive` field) to that path before exit. Atomic write
//! is the subprocess's responsibility (write to `<path>.tmp`, rename to
//! `<path>`).
//!
//! After exit, the daemon reads the file:
//!   - absent  → `Ok(None)` → caller takes `on_done` (exit==0) / `on_fail` (exit!=0)
//!   - parsed  → `Ok(Some(payload))` → caller resolves directive name → edge
//!   - garbled → `Err(Unparseable)` → caller emits `unparseable` failure

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectivePayload {
    /// Operator-chosen name; matched against `state.directives` ∪ built-in
    /// defaults at runtime.
    pub directive: String,
    /// Optional terminal-outcome override, used only when the resolved edge
    /// targets a terminal. Otherwise advisory.
    pub outcome: Option<String>,
    /// Remaining JSON fields. Exposed downstream via
    /// `{{ tasks.<state_id>.directive.<key> }}`.
    pub extra: Map<String, Value>,
}

#[derive(Debug, Error)]
pub enum SentinelError {
    #[error("read failed for {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unparseable sentinel at {path}: {detail}")]
    Unparseable { path: PathBuf, detail: String },
}

/// Read and parse the sentinel file written by the subprocess.
///
/// Returns `Ok(None)` when the file is absent (exit-code falls back to
/// on_done / on_fail). `Ok(Some(payload))` on a well-formed JSON object with
/// a `directive` field. `Err` on missing field, malformed JSON, or read error.
pub fn read_sentinel(path: &Path) -> Result<Option<DirectivePayload>, SentinelError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SentinelError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    parse_directive(path, &text).map(Some)
}

fn parse_directive(path: &Path, text: &str) -> Result<DirectivePayload, SentinelError> {
    let mut obj: Map<String, Value> =
        serde_json::from_str(text).map_err(|e| SentinelError::Unparseable {
            path: path.to_path_buf(),
            detail: format!("json parse: {e}"),
        })?;

    let directive = obj
        .remove("directive")
        .ok_or_else(|| SentinelError::Unparseable {
            path: path.to_path_buf(),
            detail: "missing required field `directive`".to_string(),
        })?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| SentinelError::Unparseable {
            path: path.to_path_buf(),
            detail: "field `directive` must be a string".to_string(),
        })?;

    let outcome = obj
        .remove("outcome")
        .and_then(|v| v.as_str().map(str::to_string));

    Ok(DirectivePayload {
        directive,
        outcome,
        extra: obj,
    })
}

/// Allocate the per-invocation sentinel path. The directives directory is
/// created on demand so the subprocess's atomic-write rename can land.
pub fn allocate_path(
    session_tempdir: &Path,
    state_id: &str,
    visit_n: u32,
) -> std::io::Result<PathBuf> {
    let dir = session_tempdir.join("directives");
    fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{state_id}.{visit_n}.json")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_sentinel(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn absent_file_returns_ok_none() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nope.json");
        assert!(read_sentinel(&p).unwrap().is_none());
    }

    #[test]
    fn valid_directive_only() {
        let dir = TempDir::new().unwrap();
        let p = write_sentinel(dir.path(), "s.json", r#"{"directive":"end"}"#);
        let payload = read_sentinel(&p).unwrap().unwrap();
        assert_eq!(payload.directive, "end");
        assert!(payload.outcome.is_none());
        assert!(payload.extra.is_empty());
    }

    #[test]
    fn directive_with_outcome_and_extra() {
        let dir = TempDir::new().unwrap();
        let p = write_sentinel(
            dir.path(),
            "s.json",
            r#"{"directive":"end","outcome":"all_done","verdict":"ok","count":3}"#,
        );
        let payload = read_sentinel(&p).unwrap().unwrap();
        assert_eq!(payload.directive, "end");
        assert_eq!(payload.outcome.as_deref(), Some("all_done"));
        assert_eq!(payload.extra.len(), 2);
        assert_eq!(payload.extra["verdict"], Value::String("ok".into()));
        assert_eq!(payload.extra["count"], Value::Number(3.into()));
    }

    #[test]
    fn missing_directive_field_errors() {
        let dir = TempDir::new().unwrap();
        let p = write_sentinel(dir.path(), "s.json", r#"{"outcome":"x"}"#);
        let err = read_sentinel(&p).unwrap_err();
        match err {
            SentinelError::Unparseable { detail, .. } => {
                assert!(detail.contains("missing required field"), "{detail}");
            }
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn directive_non_string_errors() {
        let dir = TempDir::new().unwrap();
        let p = write_sentinel(dir.path(), "s.json", r#"{"directive":123}"#);
        match read_sentinel(&p).unwrap_err() {
            SentinelError::Unparseable { detail, .. } => {
                assert!(detail.contains("must be a string"), "{detail}");
            }
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_errors() {
        let dir = TempDir::new().unwrap();
        let p = write_sentinel(dir.path(), "s.json", r#"{garbled"#);
        match read_sentinel(&p).unwrap_err() {
            SentinelError::Unparseable { detail, .. } => {
                assert!(detail.contains("json parse"), "{detail}");
            }
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn allocate_path_creates_directives_dir() {
        let dir = TempDir::new().unwrap();
        let p = allocate_path(dir.path(), "judge", 1).unwrap();
        assert_eq!(p, dir.path().join("directives").join("judge.1.json"));
        assert!(dir.path().join("directives").is_dir());
    }
}
