//! JSON-Schema for the `WORKFLOW.md` front matter (Requirement 6.1).
//!
//! The schema is intentionally permissive about the `extension` key: it is
//! accepted as an object with no further interpretation. That is what makes
//! the four reserved sub-namespaces (`extension.gates.spec.*`,
//! `extension.gates.review.*`, `extension.server.*`, `extension.distill.*`)
//! round-trip verbatim through [`super::WorkflowPolicy`] (Requirement 13.5).

use std::sync::OnceLock;

use jsonschema::{ValidationOptions, Validator};
use serde_json::{Value as JsonValue, json};

use super::WorkflowError;

/// Return the JSON-Schema object that validates `WORKFLOW.md` front matter.
///
/// Exposed for documentation and tooling (e.g., `SPEC.md` references this
/// shape). The runtime validator is compiled once via [`compiled`].
pub fn workflow_schema() -> JsonValue {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "roki WORKFLOW.md front matter",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "sandbox": {
                "type": "string",
                "enum": ["workspace-write", "read-only", "unrestricted"]
            },
            "elicitations": {
                "type": "string",
                "enum": ["reject", "allow"]
            },
            "max_turns": {
                "type": "integer",
                "minimum": 1,
                "maximum": 1000
            },
            "stall_window_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": 3600
            },
            "backoff": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "min_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 3600
                    },
                    "max_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 3600
                    }
                }
            },
            "extension": {
                "type": "object"
            }
        }
    })
}

fn compiled() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        ValidationOptions::default()
            .build(&workflow_schema())
            .expect("workflow schema must compile")
    })
}

/// Validate `instance` against the workflow schema. The first error is mapped
/// to [`WorkflowError::SchemaViolation`] with a JSON-pointer-style key path so
/// log lines name the offending key (Requirement 6.2).
pub fn validate(instance: &JsonValue) -> Result<(), WorkflowError> {
    let validator = compiled();
    if let Some(error) = validator.iter_errors(instance).next() {
        let key_path = jsonpath_to_dotted(&error.instance_path().to_string());
        return Err(WorkflowError::SchemaViolation {
            key_path,
            reason: error.to_string(),
        });
    }
    Ok(())
}

/// Convert a JSON-pointer string (e.g., `/extension/gates`) into the
/// dotted key-path form roki uses everywhere else (`extension.gates`).
/// Empty pointer becomes `<root>`.
fn jsonpath_to_dotted(pointer: &str) -> String {
    let trimmed = pointer.trim_start_matches('/');
    if trimmed.is_empty() {
        return "<root>".to_string();
    }
    trimmed.replace('/', ".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_compiles() {
        let _ = compiled();
    }

    #[test]
    fn empty_object_is_valid() {
        validate(&json!({})).expect("empty front matter is valid (defaults apply)");
    }

    #[test]
    fn unknown_top_level_field_is_rejected_with_dotted_path() {
        let err =
            validate(&json!({ "totally_unknown": 1 })).expect_err("unknown field must be rejected");
        match err {
            WorkflowError::SchemaViolation { key_path, .. } => {
                // additionalProperties violations attach to the parent object;
                // path may be `<root>` because the offending key lives there.
                assert!(
                    key_path == "<root>" || key_path.contains("totally_unknown"),
                    "unexpected key path `{key_path}`"
                );
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[test]
    fn extension_namespaces_are_opaque() {
        // The schema must accept arbitrary content under `extension` so the
        // four reserved sub-namespaces round-trip without interpretation.
        validate(&json!({
            "extension": {
                "gates": {"spec": {"x": 1}, "review": {"y": [1, 2]}},
                "server": {"bind": "0.0.0.0:1"},
                "distill": {"keep_workspace": true}
            }
        }))
        .expect("opaque extension values must be accepted");
    }

    #[test]
    fn invalid_sandbox_enum_value_is_rejected() {
        let err = validate(&json!({ "sandbox": "nope" }))
            .expect_err("invalid sandbox enum must be rejected");
        match err {
            WorkflowError::SchemaViolation { key_path, .. } => {
                assert_eq!(key_path, "sandbox");
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }
}
