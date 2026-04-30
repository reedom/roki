//! `WORKFLOW.md` policy loader.
//!
//! This module implements task 2.3 of the roki-mvp spec. It owns:
//!
//! * parsing a `WORKFLOW.md` document — YAML front matter (between `---` fences)
//!   followed by a Liquid + Markdown body;
//! * validating the parsed front matter against a JSON-Schema (Requirement 6.1);
//! * exposing the result through [`WorkflowPolicy`], whose `extension` field is
//!   typed as [`serde_json::Value`] so downstream specs can deserialize their
//!   reserved sub-slice into their own struct (Requirement 13.5);
//! * round-tripping the four canonical reserved sub-namespaces
//!   (`extension.gates.spec.*`, `extension.gates.review.*`, `extension.server.*`,
//!   `extension.distill.*`) verbatim, without interpretation;
//! * watching the on-disk file with debounce; on a successful re-parse +
//!   re-validate the in-memory policy is replaced atomically; on failure the
//!   prior valid policy is retained (last-known-good) and a structured warn
//!   event is emitted naming the offending key path (Requirements 6.3, 6.4).
//!
//! Design note: the loader does *not* render the Liquid body during load;
//! template rendering happens later (engine adapter / worker context). This
//! task only needs to capture the body verbatim alongside the policy struct,
//! and validate that the body is a syntactically well-formed Liquid template.
//! Rendering with concrete variables is downstream of task 2.3.

mod schema;
mod watcher;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

pub use schema::workflow_schema;
pub use watcher::{WatchError, WorkflowHandle};

/// Sandbox mode applied to each agent worker subprocess (Requirement 9.1, 9.2).
///
/// The default is `WorkspaceWrite`; per-repo overrides come from `WORKFLOW.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Agent may write only inside its workspace (default).
    WorkspaceWrite,
    /// Agent has read-only access.
    ReadOnly,
    /// Agent has unrestricted filesystem access (rare).
    Unrestricted,
}

impl Default for SandboxMode {
    fn default() -> Self {
        Self::WorkspaceWrite
    }
}

/// Elicitation policy applied at worker launch (Requirement 9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ElicitationsMode {
    /// Reject any elicitation request from the agent (default).
    Reject,
    /// Allow elicitations.
    Allow,
}

impl Default for ElicitationsMode {
    fn default() -> Self {
        Self::Reject
    }
}

/// Backoff knobs honoured by the orchestrator (design.md "Workflow loader").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackoffPolicy {
    /// Minimum backoff window before retrying a failed worker. Capped to no
    /// less than 10s in [`schema`] (design note).
    pub min_seconds: u64,
    /// Maximum backoff window. Capped to no more than 5min in [`schema`].
    pub max_seconds: u64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            min_seconds: 10,
            max_seconds: 300,
        }
    }
}

/// Validated `WORKFLOW.md` policy (design.md "Workflow loader").
///
/// The `extension` field is a [`serde_json::Value`] (always shaped as a JSON
/// object, see [`Self::extension_object`]). Downstream specs read their
/// reserved sub-slice via [`serde_json::from_value`] without coupling to MVP
/// types. The four canonical reserved sub-namespaces — `gates.spec.*`,
/// `gates.review.*`, `server.*`, `distill.*` — are round-tripped verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowPolicy {
    pub sandbox: SandboxMode,
    pub elicitations: ElicitationsMode,
    pub max_turns: u32,
    pub stall_window: Duration,
    pub backoff: BackoffPolicy,
    pub extension: JsonValue,
    /// The Liquid + Markdown body of the file, captured verbatim. Rendering
    /// is performed by the engine adapter at worker launch.
    pub prompt_template: String,
}

impl WorkflowPolicy {
    /// Borrow the `extension` field as a JSON object. The schema enforces
    /// that `extension` is either absent or an object, so this never panics
    /// for a policy produced by [`WorkflowLoader`].
    pub fn extension_object(&self) -> Option<&serde_json::Map<String, JsonValue>> {
        self.extension.as_object()
    }
}

/// Errors raised by the loader.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("failed to read `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("front matter missing in `{path}`: a `---` fenced YAML block is required")]
    MissingFrontMatter { path: PathBuf },

    #[error("invalid YAML front matter in `{path}` (key `{field}`): {reason}")]
    InvalidYaml {
        path: PathBuf,
        field: String,
        reason: String,
    },

    #[error("workflow body is not a valid Liquid template: {reason}")]
    InvalidLiquidBody { reason: String },

    #[error("workflow schema violation at `{key_path}`: {reason}")]
    SchemaViolation { key_path: String, reason: String },
}

impl WorkflowError {
    /// Return the offending key-path for log emission (Requirement 6.2).
    pub fn key_path(&self) -> Option<&str> {
        match self {
            Self::InvalidYaml { field, .. } => Some(field),
            Self::SchemaViolation { key_path, .. } => Some(key_path),
            Self::MissingFrontMatter { .. } => Some("<front-matter>"),
            Self::InvalidLiquidBody { .. } => Some("<body>"),
            Self::Io { .. } => None,
        }
    }
}

/// One-shot loader for a `WORKFLOW.md` file.
pub struct WorkflowLoader;

impl WorkflowLoader {
    /// Read, parse, render-check, and validate `path` into a [`WorkflowPolicy`].
    pub fn load(path: &Path) -> Result<WorkflowPolicy, WorkflowError> {
        let raw = std::fs::read_to_string(path).map_err(|source| WorkflowError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::load_from_str(&raw, path)
    }

    /// Parse + validate an in-memory `WORKFLOW.md` string. `path` is used only
    /// for error-reporting context.
    pub fn load_from_str(raw: &str, path: &Path) -> Result<WorkflowPolicy, WorkflowError> {
        let (front_matter_yaml, body) = split_front_matter(raw, path)?;

        // YAML -> serde_json::Value so that the JSON-Schema validator can run
        // against the same shape downstream specs will deserialize from.
        let front_matter_value: JsonValue = parse_yaml_to_json(front_matter_yaml, path)?;

        // Validate against the schema. `extension` is intentionally left
        // opaque (object-only) so reserved sub-namespaces round-trip verbatim.
        schema::validate(&front_matter_value)?;

        // Build the typed view from the validated value. Schema validation
        // guarantees the shape, so failures here would indicate a schema /
        // typed-struct drift bug.
        let policy = build_policy(front_matter_value, body)?;

        // Confirm the body is a syntactically valid Liquid template (parse,
        // not render). Rendering with concrete variables is downstream.
        let parser = liquid::ParserBuilder::with_stdlib()
            .build()
            .map_err(|err| WorkflowError::InvalidLiquidBody {
                reason: err.to_string(),
            })?;
        parser
            .parse(&policy.prompt_template)
            .map_err(|err| WorkflowError::InvalidLiquidBody {
                reason: err.to_string(),
            })?;

        Ok(policy)
    }

    /// Begin watching `path`. Returns a [`WorkflowHandle`] that owns a
    /// background tokio task; dropping the handle stops the watcher.
    ///
    /// On debounced filesystem events the loader re-parses and re-validates;
    /// successful results replace [`WorkflowHandle::current`] atomically;
    /// failed results retain the last-known-good policy and emit a
    /// `tracing::warn!` event with structured fields naming the bad key
    /// path (Requirement 6.4).
    pub async fn watch(path: PathBuf, debounce: Duration) -> Result<WorkflowHandle, WatchError> {
        let initial = Self::load(&path).map_err(WatchError::InitialLoad)?;
        WorkflowHandle::spawn(path, Arc::new(initial), debounce).await
    }
}

/// Split `raw` into `(front_matter_yaml, body)` using the `---` fence pair.
fn split_front_matter<'a>(raw: &'a str, path: &Path) -> Result<(&'a str, &'a str), WorkflowError> {
    let trimmed = raw.trim_start_matches('\u{feff}');
    let trimmed = trimmed.trim_start_matches(['\n', '\r']);
    let after_open = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
        .ok_or_else(|| WorkflowError::MissingFrontMatter {
            path: path.to_path_buf(),
        })?;
    let close_idx =
        find_close_fence(after_open).ok_or_else(|| WorkflowError::MissingFrontMatter {
            path: path.to_path_buf(),
        })?;
    let yaml = &after_open[..close_idx];
    // Skip past the closing fence (`---` plus its trailing newline if any).
    let after_close = &after_open[close_idx..];
    let body = after_close
        .strip_prefix("---\n")
        .or_else(|| after_close.strip_prefix("---\r\n"))
        .or_else(|| after_close.strip_prefix("---"))
        .unwrap_or("");
    Ok((yaml, body))
}

/// Find the byte offset of the closing `---` fence inside `after_open`.
///
/// The fence must appear at the start of a line. We scan line-by-line so a
/// `---` appearing inside a YAML scalar does not falsely close the block.
fn find_close_fence(after_open: &str) -> Option<usize> {
    let mut cursor = 0usize;
    for line in after_open.split_inclusive('\n') {
        let line_no_eol = line.trim_end_matches(['\n', '\r']);
        if line_no_eol == "---" {
            return Some(cursor);
        }
        cursor += line.len();
    }
    None
}

/// Parse a YAML string into a [`serde_json::Value`].
fn parse_yaml_to_json(yaml: &str, path: &Path) -> Result<JsonValue, WorkflowError> {
    let yaml_value: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|err| WorkflowError::InvalidYaml {
            path: path.to_path_buf(),
            field: yaml_error_field(&err).unwrap_or_else(|| "<root>".to_string()),
            reason: err.to_string(),
        })?;
    serde_json::to_value(yaml_value).map_err(|err| WorkflowError::InvalidYaml {
        path: path.to_path_buf(),
        field: "<root>".to_string(),
        reason: err.to_string(),
    })
}

/// Best-effort extraction of the offending key from a `serde_yaml::Error`.
///
/// `serde_yaml`'s error message includes the location and surrounding context
/// but does not directly expose the key name; we fall back to `<root>` when
/// the error does not name a field.
fn yaml_error_field(err: &serde_yaml::Error) -> Option<String> {
    let rendered = err.to_string();
    // Look for patterns like "missing field `foo`" or "unknown field `foo`".
    for marker in ["missing field `", "unknown field `", "invalid type for `"] {
        if let Some(start) = rendered.find(marker) {
            let after = &rendered[start + marker.len()..];
            if let Some(end) = after.find('`') {
                return Some(after[..end].to_string());
            }
        }
    }
    None
}

/// Build a typed [`WorkflowPolicy`] from a schema-validated JSON value.
fn build_policy(value: JsonValue, body: &str) -> Result<WorkflowPolicy, WorkflowError> {
    let object = value
        .as_object()
        .ok_or_else(|| WorkflowError::SchemaViolation {
            key_path: "<root>".to_string(),
            reason: "front matter must be a YAML mapping".to_string(),
        })?;

    let sandbox = match object.get("sandbox") {
        Some(JsonValue::String(s)) => parse_enum::<SandboxMode>("sandbox", s)?,
        None => SandboxMode::default(),
        Some(other) => {
            return Err(WorkflowError::SchemaViolation {
                key_path: "sandbox".to_string(),
                reason: format!("expected string, got {}", json_type_name(other)),
            });
        }
    };

    let elicitations = match object.get("elicitations") {
        Some(JsonValue::String(s)) => parse_enum::<ElicitationsMode>("elicitations", s)?,
        None => ElicitationsMode::default(),
        Some(other) => {
            return Err(WorkflowError::SchemaViolation {
                key_path: "elicitations".to_string(),
                reason: format!("expected string, got {}", json_type_name(other)),
            });
        }
    };

    let max_turns = object
        .get("max_turns")
        .and_then(JsonValue::as_u64)
        .map(|n| n as u32)
        .unwrap_or(40);

    let stall_window_seconds = object
        .get("stall_window_seconds")
        .and_then(JsonValue::as_u64)
        .unwrap_or(180);
    let stall_window = Duration::from_secs(stall_window_seconds);

    let backoff = match object.get("backoff") {
        Some(v) => serde_json::from_value::<BackoffPolicy>(v.clone()).map_err(|err| {
            WorkflowError::SchemaViolation {
                key_path: "backoff".to_string(),
                reason: err.to_string(),
            }
        })?,
        None => BackoffPolicy::default(),
    };

    // Reserve `extension` as an object (or default to empty). Reserved
    // sub-namespaces inside `extension` are NOT interpreted; they round-trip
    // verbatim so downstream specs can deserialize their own slice.
    let extension = match object.get("extension") {
        Some(JsonValue::Object(_)) => object.get("extension").cloned().unwrap(),
        Some(JsonValue::Null) | None => JsonValue::Object(Default::default()),
        Some(other) => {
            return Err(WorkflowError::SchemaViolation {
                key_path: "extension".to_string(),
                reason: format!("expected object, got {}", json_type_name(other)),
            });
        }
    };

    Ok(WorkflowPolicy {
        sandbox,
        elicitations,
        max_turns,
        stall_window,
        backoff,
        extension,
        prompt_template: body.to_string(),
    })
}

fn json_type_name(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

trait ParseEnum: Sized {
    fn parse(input: &str) -> Option<Self>;
    fn allowed() -> &'static [&'static str];
}

impl ParseEnum for SandboxMode {
    fn parse(input: &str) -> Option<Self> {
        match input {
            "workspace-write" => Some(Self::WorkspaceWrite),
            "read-only" => Some(Self::ReadOnly),
            "unrestricted" => Some(Self::Unrestricted),
            _ => None,
        }
    }
    fn allowed() -> &'static [&'static str] {
        &["workspace-write", "read-only", "unrestricted"]
    }
}

impl ParseEnum for ElicitationsMode {
    fn parse(input: &str) -> Option<Self> {
        match input {
            "reject" => Some(Self::Reject),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }
    fn allowed() -> &'static [&'static str] {
        &["reject", "allow"]
    }
}

fn parse_enum<T: ParseEnum>(field: &str, raw: &str) -> Result<T, WorkflowError> {
    T::parse(raw).ok_or_else(|| WorkflowError::SchemaViolation {
        key_path: field.to_string(),
        reason: format!("expected one of {:?}, got `{raw}`", T::allowed()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path() -> PathBuf {
        PathBuf::from("WORKFLOW.md")
    }

    fn valid_workflow() -> &'static str {
        r#"---
sandbox: workspace-write
elicitations: reject
max_turns: 30
stall_window_seconds: 120
backoff:
  min_seconds: 10
  max_seconds: 300
extension:
  gates:
    spec:
      required_phases: [requirements, design, tasks]
    review:
      block_on_findings: true
  server:
    bind: "127.0.0.1:7777"
  distill:
    output_dir: "distill/"
---
# Workflow prompt body

Render with {{ issue.id }} and {{ repo.id }}.
"#
    }

    #[test]
    fn valid_workflow_md_loads_with_all_typed_fields_populated() {
        let policy = WorkflowLoader::load_from_str(valid_workflow(), &fixture_path())
            .expect("valid workflow must load");
        assert_eq!(policy.sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(policy.elicitations, ElicitationsMode::Reject);
        assert_eq!(policy.max_turns, 30);
        assert_eq!(policy.stall_window, Duration::from_secs(120));
        assert_eq!(
            policy.backoff,
            BackoffPolicy {
                min_seconds: 10,
                max_seconds: 300,
            }
        );
        assert!(policy.prompt_template.contains("Render with"));
    }

    #[test]
    fn extension_namespaces_round_trip_byte_for_byte() {
        // Observable-completion #1 (unit half): all four reserved sub-namespaces
        // round-trip verbatim through the policy struct without interpretation.
        let policy = WorkflowLoader::load_from_str(valid_workflow(), &fixture_path())
            .expect("valid workflow must load");

        let ext = policy
            .extension_object()
            .expect("extension must be an object");

        // gates.spec.*
        let spec = ext
            .get("gates")
            .and_then(|g| g.get("spec"))
            .expect("gates.spec must round-trip");
        assert_eq!(
            spec.get("required_phases").expect("required_phases"),
            &serde_json::json!(["requirements", "design", "tasks"])
        );

        // gates.review.*
        let review = ext
            .get("gates")
            .and_then(|g| g.get("review"))
            .expect("gates.review must round-trip");
        assert_eq!(
            review.get("block_on_findings").expect("block_on_findings"),
            &serde_json::json!(true)
        );

        // server.*
        let server = ext.get("server").expect("server must round-trip");
        assert_eq!(
            server.get("bind").expect("server.bind"),
            &serde_json::json!("127.0.0.1:7777")
        );

        // distill.*
        let distill = ext.get("distill").expect("distill must round-trip");
        assert_eq!(
            distill.get("output_dir").expect("distill.output_dir"),
            &serde_json::json!("distill/")
        );
    }

    #[test]
    fn extension_field_is_serde_json_value() {
        // Compile-time check: assignment from `serde_json::Value` works.
        let policy = WorkflowLoader::load_from_str(valid_workflow(), &fixture_path())
            .expect("valid workflow must load");
        let _v: &serde_json::Value = &policy.extension;
        assert!(policy.extension.is_object());
    }

    #[test]
    fn missing_front_matter_is_rejected() {
        let no_front_matter = "# just a markdown body\n";
        let err = WorkflowLoader::load_from_str(no_front_matter, &fixture_path())
            .expect_err("missing front matter must be rejected");
        assert!(matches!(err, WorkflowError::MissingFrontMatter { .. }));
    }

    #[test]
    fn invalid_yaml_returns_typed_error_naming_offending_field() {
        let invalid_yaml = "---\nmax_turns: : not-valid\n---\nbody\n";
        let err = WorkflowLoader::load_from_str(invalid_yaml, &fixture_path())
            .expect_err("invalid YAML must be rejected");
        assert!(matches!(err, WorkflowError::InvalidYaml { .. }));
        // key_path is best-effort; for syntax errors it falls back to <root>.
        assert!(err.key_path().is_some());
    }

    #[test]
    fn invalid_schema_value_returns_error_naming_offending_field() {
        // `sandbox` must be one of the allowed enum values.
        let bad_sandbox = r#"---
sandbox: nope-not-a-real-mode
elicitations: reject
---
body
"#;
        let err = WorkflowLoader::load_from_str(bad_sandbox, &fixture_path())
            .expect_err("invalid sandbox must be rejected");
        assert!(matches!(err, WorkflowError::SchemaViolation { .. }));
        assert_eq!(err.key_path(), Some("sandbox"));
    }

    #[test]
    fn extension_must_be_an_object() {
        let bad_extension = r#"---
sandbox: workspace-write
elicitations: reject
extension: "not-an-object"
---
body
"#;
        let err = WorkflowLoader::load_from_str(bad_extension, &fixture_path())
            .expect_err("non-object extension must be rejected");
        assert_eq!(err.key_path(), Some("extension"));
    }

    #[test]
    fn body_must_be_valid_liquid() {
        // An unclosed liquid tag is a parse error.
        let bad_body = r#"---
sandbox: workspace-write
elicitations: reject
---
{% if oops
"#;
        let err = WorkflowLoader::load_from_str(bad_body, &fixture_path())
            .expect_err("invalid liquid body must be rejected");
        assert!(matches!(err, WorkflowError::InvalidLiquidBody { .. }));
    }

    #[test]
    fn body_is_preserved_verbatim() {
        let policy = WorkflowLoader::load_from_str(valid_workflow(), &fixture_path())
            .expect("valid workflow must load");
        assert!(
            policy
                .prompt_template
                .contains("Render with {{ issue.id }}")
        );
        assert!(policy.prompt_template.contains("{{ repo.id }}"));
    }

    #[test]
    fn unknown_extension_keys_round_trip_unchanged() {
        // Requirement 13.5: the loader does not interpret reserved namespaces;
        // arbitrary downstream-spec keys must round-trip verbatim.
        let with_unknowns = r#"---
sandbox: workspace-write
elicitations: reject
extension:
  gates:
    spec:
      future_unknown_key: ["not", "interpreted", 42]
  server:
    new_telemetry_block:
      sample_rate: 0.25
---
body
"#;
        let policy = WorkflowLoader::load_from_str(with_unknowns, &fixture_path())
            .expect("unknown extension keys must be accepted");
        let ext = policy.extension_object().expect("object");
        let val = ext
            .get("gates")
            .and_then(|g| g.get("spec"))
            .and_then(|s| s.get("future_unknown_key"))
            .expect("future key round-trip");
        assert_eq!(val, &serde_json::json!(["not", "interpreted", 42]));
        let sample_rate = ext
            .get("server")
            .and_then(|s| s.get("new_telemetry_block"))
            .and_then(|t| t.get("sample_rate"))
            .expect("server sample rate round-trip");
        assert_eq!(sample_rate, &serde_json::json!(0.25));
    }
}
