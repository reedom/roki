//! `WORKFLOW.md` JSON-Schema validation, default application, and legacy-key
//! sweep.
//!
//! Boundary contract: this module accepts a [`ParsedWorkflow`] from
//! [`crate::workflow::parse`] and produces a [`WorkflowPolicy`] suitable for
//! the orchestrator session and phase-subprocess adapters. Reserved
//! namespaces (`extension.orchestrator.*`, `extension.phase.<name>.*`,
//! `extension.server.*`) round-trip even when the loader does not interpret
//! their contents — downstream specs read their own keys directly off
//! [`WorkflowPolicy::raw_unknowns`].
//!
//! Spec refs: requirements.md Req 2.11, 2.12, 6.2, 6.4, 6.5, 6.7, 13.5;
//! docs/reference/config.md Reserved extension namespaces.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use jsonschema::{Validator, validator_for};
use serde_json::{Value, json};
use thiserror::Error;

use super::parse::ParsedWorkflow;

/// Default model for `extension.orchestrator.model` per
/// `docs/reference/config.md`.
pub const DEFAULT_MODEL: &str = "claude-opus-4-7";
/// Default `extension.orchestrator.effort` per `docs/reference/config.md`.
pub const DEFAULT_EFFORT: &str = "middle";
/// Default `extension.orchestrator.max_phases` (lowered from 20 to 15).
pub const DEFAULT_MAX_PHASES: u32 = 15;
/// Default orchestrator-stall window in seconds.
pub const DEFAULT_STALL_SECONDS: u32 = 600;

/// Default allowlist passed to `--settings` for the orchestrator. The
/// canonical surface is Linear MCP (write) + `Read` + `Bash` per Req 5.1 /
/// Req 7.1; the daemon represents Linear MCP via the wildcard `mcp__linear*`
/// matcher so any operator-installed Linear MCP server is admitted.
fn default_allowed_tools() -> Vec<String> {
    vec![
        "mcp__linear*".to_owned(),
        "Read".to_owned(),
        "Bash".to_owned(),
    ]
}

/// JSON-Schema document for the reserved namespaces in `WORKFLOW.md` front
/// matter. Anything outside `extension.*` (and the legacy keys) is left to
/// downstream consumers.
pub static WORKFLOW_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "extension": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "orchestrator": {
                        "type": "object",
                        "additionalProperties": true,
                        "properties": {
                            "model":   { "type": "string", "minLength": 1 },
                            "effort":  { "type": "string", "enum": ["low", "middle", "high"] },
                            "max_phases":    { "type": "integer", "minimum": 1, "maximum": 100 },
                            "allowed_tools": {
                                "type": "array",
                                "items": { "type": "string", "minLength": 1 }
                            },
                            "stall_seconds": { "type": "integer", "minimum": 1, "maximum": 3600 }
                        }
                    },
                    "phase": {
                        "type": "object",
                        "additionalProperties": {
                            "type": "object",
                            "additionalProperties": true,
                            "properties": {
                                "command":      { "type": "string", "minLength": 1 },
                                "max_turns":     { "type": "integer", "minimum": 1, "maximum": 200 },
                                "stall_seconds": { "type": "integer", "minimum": 1, "maximum": 3600 },
                                "max_attempts":  { "type": "integer", "minimum": 1, "maximum": 10 }
                            }
                        }
                    },
                    "server": {
                        "type": "object",
                        "additionalProperties": true
                    }
                }
            }
        }
    })
});

/// Resolved orchestrator-namespace policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorConfig {
    pub model: String,
    pub effort: Effort,
    pub max_phases: u32,
    pub allowed_tools: Vec<String>,
    pub stall_seconds: u32,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_owned(),
            effort: Effort::Middle,
            max_phases: DEFAULT_MAX_PHASES,
            allowed_tools: default_allowed_tools(),
            stall_seconds: DEFAULT_STALL_SECONDS,
        }
    }
}

/// Extended-thinking budget for the orchestrator session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Middle,
    High,
}

/// Per-phase override values (additive over the catalog default). The prompt
/// template form (`prompt_template_<phase>`) is detected separately via
/// [`WorkflowPolicy::blocks`] — callers that want to know which override
/// shape is in effect for a phase consult both surfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhaseConfig {
    pub command: Option<String>,
    pub max_turns: Option<u32>,
    pub stall_seconds: Option<u32>,
    pub max_attempts: Option<u32>,
}

/// Final loader output: typed reserved-namespace slices plus the named
/// template blocks plus the round-tripped opaque `extension.server.*` blob.
/// Engine adapters consume `Arc<WorkflowPolicy>` (see
/// [`crate::engine::phase_subprocess::catalog::WorkflowPolicyHandle`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowPolicy {
    pub orchestrator: OrchestratorConfig,
    pub phases: BTreeMap<String, PhaseConfig>,
    pub server: Value,
    pub blocks: BTreeMap<String, String>,
    /// `extension.*` unknowns under reserved namespaces preserved verbatim
    /// for downstream specs to interpret. Object shape: `{"extension": {...}}`
    /// with consumed slices removed.
    pub raw_unknowns: Value,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SchemaError {
    /// JSON-Schema validation failure; the `key_path` field is the dotted
    /// path produced by the validator (or `<root>` if the violation is at
    /// the root).
    #[error("WORKFLOW.md schema validation failed at `{key_path}`: {message}")]
    Validation { key_path: String, message: String },

    /// Both `extension.phase.<name>.command` and `prompt_template_<name>`
    /// declared for the same phase.
    #[error(
        "phase `{phase}` declares both `extension.phase.{phase}.command` and \
         the named template block `prompt_template_{phase}`; the two \
         override forms are mutually exclusive (Req 6.7)"
    )]
    BothOverrideForms { phase: String },

    /// Legacy namespace key encountered.
    #[error(
        "legacy WORKFLOW.md key `{key}` is no longer supported; both \
         classification and Linear writes are now performed by the \
         orchestrator session — see docs/fr/19-orchestrator-session.md"
    )]
    LegacyKey { key: String },

    /// Unknown phase under `extension.phase.<name>` (not in the legal seven).
    #[error(
        "unknown phase override `extension.phase.{phase}.*`; legal phase \
         names: classify, implement, review, validate, open_pr, ci_fix, \
         finalize_review"
    )]
    UnknownPhase { phase: String },

    /// `extension.phase.<name>` is not an object.
    #[error("`extension.phase.{phase}` must be a table")]
    PhaseNotTable { phase: String },
}

/// Validate a [`ParsedWorkflow`] against [`WORKFLOW_SCHEMA`], apply canonical
/// defaults, and produce a [`WorkflowPolicy`].
///
/// Validation order matches the operator-visible refusal hierarchy:
/// 1. Legacy keys (Req 2.12) — fail fast even before schema run.
/// 2. JSON-Schema (Req 6.2) — emits the offending key path verbatim.
/// 3. Both-override-forms cross-check (Req 6.7).
/// 4. Unknown-phase rejection.
pub fn validate(parsed: ParsedWorkflow) -> Result<WorkflowPolicy, SchemaError> {
    let ParsedWorkflow {
        front_matter,
        blocks,
    } = parsed;

    // Normalize front matter to an object so downstream traversals do not
    // need to special-case `null` (empty YAML) or scalar shapes.
    let front_matter = match front_matter {
        Value::Object(_) => front_matter,
        Value::Null => Value::Object(serde_json::Map::new()),
        other => {
            return Err(SchemaError::Validation {
                key_path: "<root>".to_owned(),
                message: format!("front matter must be a table; got {other:?}"),
            });
        }
    };

    reject_legacy_keys(&front_matter)?;
    run_json_schema(&front_matter)?;
    let orchestrator = extract_orchestrator(&front_matter)?;
    let phases = extract_phases(&front_matter)?;
    let server = extract_server(&front_matter);
    let raw_unknowns = strip_consumed_namespaces(&front_matter);

    cross_check_override_forms(&phases, &blocks)?;

    Ok(WorkflowPolicy {
        orchestrator,
        phases,
        server,
        blocks,
        raw_unknowns,
    })
}

fn reject_legacy_keys(front_matter: &Value) -> Result<(), SchemaError> {
    let table = match front_matter.as_object() {
        Some(t) => t,
        None => return Ok(()),
    };

    if let Some(judge) = table.get("judge")
        && judge.get("model").is_some()
    {
        return Err(SchemaError::LegacyKey {
            key: "[judge].model".to_owned(),
        });
    }

    if let Some(extension) = table.get("extension").and_then(Value::as_object) {
        if extension.contains_key("linear_updater") {
            return Err(SchemaError::LegacyKey {
                key: "extension.linear_updater".to_owned(),
            });
        }
        if extension.contains_key("distill") {
            return Err(SchemaError::LegacyKey {
                key: "extension.distill".to_owned(),
            });
        }
        if let Some(gates) = extension.get("gates").and_then(Value::as_object) {
            if gates.contains_key("spec") {
                return Err(SchemaError::LegacyKey {
                    key: "extension.gates.spec".to_owned(),
                });
            }
            if gates.contains_key("review") {
                return Err(SchemaError::LegacyKey {
                    key: "extension.gates.review".to_owned(),
                });
            }
            // `extension.gates` itself with no recognized children is still
            // legacy — refuse the namespace wholesale.
            return Err(SchemaError::LegacyKey {
                key: "extension.gates".to_owned(),
            });
        }
    }

    Ok(())
}

fn run_json_schema(front_matter: &Value) -> Result<(), SchemaError> {
    let validator: Validator = validator_for(&WORKFLOW_SCHEMA).map_err(|e| {
        // Schema author error: surface as a root-level validation failure so
        // operators see something rather than a panic.
        SchemaError::Validation {
            key_path: "<root>".to_owned(),
            message: format!("internal schema build error: {e}"),
        }
    })?;
    if let Err(error) = validator.validate(front_matter) {
        let key_path = error.instance_path().to_string();
        let key_path = if key_path.is_empty() {
            "<root>".to_owned()
        } else {
            // jsonschema yields `/extension/orchestrator/max_phases`-style
            // paths; rewrite to dotted form expected by docs/reference/config.md.
            key_path.trim_start_matches('/').replace('/', ".")
        };
        return Err(SchemaError::Validation {
            key_path,
            message: error.to_string(),
        });
    }
    Ok(())
}

fn extract_orchestrator(front_matter: &Value) -> Result<OrchestratorConfig, SchemaError> {
    let mut cfg = OrchestratorConfig::default();
    let Some(orch) = front_matter
        .get("extension")
        .and_then(|v| v.get("orchestrator"))
    else {
        return Ok(cfg);
    };

    if let Some(model) = orch.get("model").and_then(Value::as_str) {
        cfg.model = model.to_owned();
    }
    if let Some(effort) = orch.get("effort").and_then(Value::as_str) {
        cfg.effort = match effort {
            "low" => Effort::Low,
            "middle" => Effort::Middle,
            "high" => Effort::High,
            // Schema enum guard makes this branch unreachable in practice;
            // keep the explicit fallback so a schema bug does not panic.
            _ => Effort::Middle,
        };
    }
    if let Some(max_phases) = orch.get("max_phases").and_then(Value::as_u64) {
        cfg.max_phases = max_phases as u32;
    }
    if let Some(stall) = orch.get("stall_seconds").and_then(Value::as_u64) {
        cfg.stall_seconds = stall as u32;
    }
    if let Some(tools) = orch.get("allowed_tools").and_then(Value::as_array) {
        cfg.allowed_tools = tools
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
    }

    Ok(cfg)
}

fn extract_phases(front_matter: &Value) -> Result<BTreeMap<String, PhaseConfig>, SchemaError> {
    let mut phases: BTreeMap<String, PhaseConfig> = BTreeMap::new();
    let Some(phase_table) = front_matter
        .get("extension")
        .and_then(|v| v.get("phase"))
        .and_then(Value::as_object)
    else {
        return Ok(phases);
    };

    for (name, value) in phase_table {
        if !LEGAL_PHASES.contains(&name.as_str()) {
            return Err(SchemaError::UnknownPhase {
                phase: name.clone(),
            });
        }
        let table = value.as_object().ok_or_else(|| SchemaError::PhaseNotTable {
            phase: name.clone(),
        })?;
        let mut cfg = PhaseConfig::default();
        if let Some(cmd) = table.get("command").and_then(Value::as_str) {
            cfg.command = Some(cmd.to_owned());
        }
        if let Some(mt) = table.get("max_turns").and_then(Value::as_u64) {
            cfg.max_turns = Some(mt as u32);
        }
        if let Some(ss) = table.get("stall_seconds").and_then(Value::as_u64) {
            cfg.stall_seconds = Some(ss as u32);
        }
        if let Some(ma) = table.get("max_attempts").and_then(Value::as_u64) {
            cfg.max_attempts = Some(ma as u32);
        }
        phases.insert(name.clone(), cfg);
    }

    Ok(phases)
}

fn extract_server(front_matter: &Value) -> Value {
    front_matter
        .get("extension")
        .and_then(|v| v.get("server"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
}

/// Produce a copy of the front matter with the consumed reserved-namespace
/// slices (`extension.orchestrator`, `extension.phase`) removed; everything
/// else under `extension.*` and the rest of the table is kept verbatim. The
/// result is what downstream specs that may extend the schema later receive
/// without re-parsing the source.
fn strip_consumed_namespaces(front_matter: &Value) -> Value {
    let mut clone = front_matter.clone();
    if let Some(table) = clone.as_object_mut()
        && let Some(extension) = table.get_mut("extension").and_then(Value::as_object_mut)
    {
        extension.remove("orchestrator");
        extension.remove("phase");
        // `extension.server.*` deliberately stays in raw_unknowns so
        // roki-observability and other downstream specs can find it.
    }
    clone
}

fn cross_check_override_forms(
    phases: &BTreeMap<String, PhaseConfig>,
    blocks: &BTreeMap<String, String>,
) -> Result<(), SchemaError> {
    for (name, cfg) in phases {
        if cfg.command.is_some() {
            let block_name = format!("prompt_template_{name}");
            if blocks.contains_key(&block_name) {
                return Err(SchemaError::BothOverrideForms {
                    phase: name.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Cheap-clone handle the engine adapters carry. Type alias so existing call
/// sites that hold `WorkflowPolicyHandle` keep their shape; they simply
/// upgrade from the stub unit struct to a shared `Arc<WorkflowPolicy>`.
pub type WorkflowPolicyHandle = Arc<WorkflowPolicy>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::parse::parse_str;

    fn baseline_workflow(extra_front_matter: &str) -> String {
        format!(
            "---\n{extra_front_matter}---\n\
             ## prompt_template_orchestrator\norch\n\
             \n## prompt_template_implement_direct\nimpl\n\
             \n## prompt_template_validate_direct\nval\n\
             \n## prompt_template_open_pr\nopen\n",
        )
    }

    #[test]
    fn applies_canonical_defaults_when_keys_omitted() {
        let parsed = parse_str(&baseline_workflow("")).unwrap();
        let policy = validate(parsed).expect("defaults must apply");
        assert_eq!(policy.orchestrator.model, DEFAULT_MODEL);
        assert_eq!(policy.orchestrator.effort, Effort::Middle);
        assert_eq!(policy.orchestrator.max_phases, DEFAULT_MAX_PHASES);
        assert_eq!(policy.orchestrator.stall_seconds, DEFAULT_STALL_SECONDS);
        assert_eq!(policy.orchestrator.allowed_tools, default_allowed_tools());
        assert!(policy.phases.is_empty());
    }

    #[test]
    fn out_of_range_max_phases_is_refused_with_key_path() {
        let extra = "extension:\n  orchestrator:\n    max_phases: 500\n";
        let parsed = parse_str(&baseline_workflow(extra)).unwrap();
        let err = validate(parsed).unwrap_err();
        match err {
            SchemaError::Validation { ref key_path, .. } => {
                assert!(
                    key_path.contains("extension.orchestrator.max_phases"),
                    "expected dotted key path, got {key_path}",
                );
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn both_override_forms_for_same_phase_refused() {
        // `extension.phase.review.command` plus `prompt_template_review` body.
        let extra = "extension:\n  phase:\n    review:\n      command: \"/custom-review\"\n";
        let body = format!(
            "---\n{extra}---\n\
             ## prompt_template_orchestrator\norch\n\
             \n## prompt_template_implement_direct\nimpl\n\
             \n## prompt_template_validate_direct\nval\n\
             \n## prompt_template_open_pr\nopen\n\
             \n## prompt_template_review\nreview override block\n",
        );
        let parsed = parse_str(&body).unwrap();
        let err = validate(parsed).unwrap_err();
        assert_eq!(
            err,
            SchemaError::BothOverrideForms {
                phase: "review".to_owned()
            },
        );
    }

    #[test]
    fn legacy_extension_gates_spec_refused() {
        let extra = "extension:\n  gates:\n    spec:\n      foo: 1\n";
        let parsed = parse_str(&baseline_workflow(extra)).unwrap();
        let err = validate(parsed).unwrap_err();
        assert_eq!(
            err,
            SchemaError::LegacyKey {
                key: "extension.gates.spec".to_owned(),
            },
        );
    }

    #[test]
    fn unknown_extension_server_key_round_trips() {
        let extra = "extension:\n  server:\n    foo: bar\n";
        let parsed = parse_str(&baseline_workflow(extra)).unwrap();
        let policy = validate(parsed).expect("unknown server keys must round-trip");
        // Round-tripped both via the typed slice...
        assert_eq!(
            policy.server,
            json!({"foo": "bar"}),
            "server slice must carry through unchanged",
        );
        // ...and via the raw_unknowns mirror, so downstream specs that join
        // on the full front-matter shape still see the key.
        let unknowns_server = policy
            .raw_unknowns
            .get("extension")
            .and_then(|v| v.get("server"))
            .cloned()
            .unwrap_or(Value::Null);
        assert_eq!(unknowns_server, json!({"foo": "bar"}));
    }

    #[test]
    fn legacy_judge_model_refused() {
        let extra = "judge:\n  model: \"claude-sonnet-4\"\n";
        let parsed = parse_str(&baseline_workflow(extra)).unwrap();
        let err = validate(parsed).unwrap_err();
        assert_eq!(
            err,
            SchemaError::LegacyKey {
                key: "[judge].model".to_owned(),
            },
        );
    }

    #[test]
    fn unknown_phase_name_refused() {
        let extra = "extension:\n  phase:\n    bogus:\n      command: \"x\"\n";
        let parsed = parse_str(&baseline_workflow(extra)).unwrap();
        let err = validate(parsed).unwrap_err();
        assert_eq!(
            err,
            SchemaError::UnknownPhase {
                phase: "bogus".to_owned(),
            },
        );
    }

    #[test]
    fn legal_phase_command_only_is_accepted() {
        // No matching prompt_template_<phase> block → no cross-form conflict.
        let extra = "extension:\n  phase:\n    review:\n      command: \"/custom-review\"\n      max_turns: 25\n";
        let parsed = parse_str(&baseline_workflow(extra)).unwrap();
        let policy = validate(parsed).expect("command-only override is legal");
        let review = policy
            .phases
            .get("review")
            .expect("review phase override carried");
        assert_eq!(review.command.as_deref(), Some("/custom-review"));
        assert_eq!(review.max_turns, Some(25));
    }
}
