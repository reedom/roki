//! Engine type vocabulary.
//!
//! Variant naming mirrors the FR 01 directive schema: pre returns
//! `run` / `end`; post returns `pre` / `run` / `end`. `FailureKind` enumerates
//! every directive-level failure the engine can route in slice 1.

#![allow(dead_code)]

use std::path::PathBuf;

use serde::Deserialize;

/// Which phase position the engine is executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    Pre,
    Run,
    Post,
}

impl PhaseKind {
    /// Lowercase canonical name used for capture file prefixes and tracing.
    pub fn as_str(self) -> &'static str {
        match self {
            PhaseKind::Pre => "pre",
            PhaseKind::Run => "run",
            PhaseKind::Post => "post",
        }
    }
}

/// Subprocess wire shape per phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseShape {
    /// Long-lived AI subprocess reused across all pre/post turns of the cycle.
    Session,
    /// One-shot subprocess per phase invocation.
    Command,
}

impl PhaseShape {
    pub fn as_str(self) -> &'static str {
        match self {
            PhaseShape::Session => "session",
            PhaseShape::Command => "command",
        }
    }
}

/// Operator-authored body for one phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseBody {
    /// Inline `cmd = "<shell line>"`. Rendered, then run as `sh -c <rendered>`.
    /// stdin is closed immediately.
    InlineCmd { cmd: String },
    /// Inline `prompt = "<text>"`. Rendered as the stdin body. Argv comes from
    /// `[default.ai.command].cli` (or a frontmatter override, but inline form
    /// has no frontmatter, so always the default).
    InlinePrompt { prompt: String },
    /// `path = "workflow/<file>.md"`. Resolved at config-load time against
    /// the workflow file's parent directory.
    Path {
        path: PathBuf,
        cli_override: Option<String>,
        /// Resolved from the .md frontmatter `session:` field.
        /// Defaults to `Session` when the field is absent.
        shape: PhaseShape,
        /// Resolved from the .md frontmatter `stall_seconds:` field.
        /// `None` means "fall back to the shape default in `[default.ai.*].stall_seconds`".
        stall_seconds: Option<u32>,
    },
}

impl PhaseBody {
    /// Wire shape this phase body resolves to.
    pub fn shape(&self) -> PhaseShape {
        match self {
            PhaseBody::InlineCmd { .. } => PhaseShape::Command,
            PhaseBody::InlinePrompt { .. } => PhaseShape::Session,
            PhaseBody::Path { shape, .. } => *shape,
        }
    }

    /// Per-file `stall_seconds` override, or `None` for shape-default.
    pub fn stall_seconds_override(&self) -> Option<u32> {
        match self {
            PhaseBody::InlineCmd { .. } | PhaseBody::InlinePrompt { .. } => None,
            PhaseBody::Path { stall_seconds, .. } => *stall_seconds,
        }
    }
}

/// Pre-phase legal directive set: `run` or `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreDirective {
    Run,
    End,
}

impl PreDirective {
    pub fn try_from_str(value: &str) -> Option<Self> {
        match value {
            "run" => Some(PreDirective::Run),
            "end" => Some(PreDirective::End),
            _ => None,
        }
    }
}

/// Post-phase legal directive set: `pre`, `run`, or `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostDirective {
    Pre,
    Run,
    End,
}

impl PostDirective {
    pub fn try_from_str(value: &str) -> Option<Self> {
        match value {
            "pre" => Some(PostDirective::Pre),
            "run" => Some(PostDirective::Run),
            "end" => Some(PostDirective::End),
            _ => None,
        }
    }
}

/// One phase invocation's outcome forwarded to `engine::cycle`.
#[derive(Debug, Clone)]
pub enum PhaseOutcome {
    PreDirective {
        directive: PreDirective,
        payload: serde_json::Value,
    },
    PostDirective {
        directive: PostDirective,
        payload: serde_json::Value,
    },
    RunDone {
        exit_code: i32,
        duration_seconds: u64,
    },
    Failure {
        kind: FailureKind,
    },
}

impl PhaseOutcome {
    /// Static name of the variant. Used in `PhaseInfraError::ExecutorContract`
    /// when the cycle driver receives an outcome variant the phase does not
    /// produce, so the operator log identifies which variant tripped the
    /// executor contract.
    pub fn variant_name(&self) -> &'static str {
        match self {
            PhaseOutcome::PreDirective { .. } => "PreDirective",
            PhaseOutcome::PostDirective { .. } => "PostDirective",
            PhaseOutcome::RunDone { .. } => "RunDone",
            PhaseOutcome::Failure { .. } => "Failure",
        }
    }
}

/// Directive-level failure kinds. Distinct from `PhaseInfraError`, which
/// represents infrastructure-level failures that escape the cycle as a
/// `Result::Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Pre/Post: stdout has no JSON object, or the last JSON object lacks
    /// `directive`.
    Unparseable,
    /// Pre/Post: `directive` value outside the legal set for the phase.
    SchemaDrift,
    /// Pre/Post: non-zero exit and stdout has no parseable JSON object.
    ProcessCrash,
    /// Liquid render of argv or stdin body failed before launch.
    TemplateError,
    /// Post returned `pre` or `run` while `iter == max_iterations`.
    IterExhausted,
    /// Stdout silent for `stall_seconds`; supervisor SIGTERMed (and SIGKILLed
    /// after grace if necessary). Applies to both shapes.
    Stall,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::Unparseable => "unparseable",
            FailureKind::SchemaDrift => "schema_drift",
            FailureKind::ProcessCrash => "process_crash",
            FailureKind::TemplateError => "template_error",
            FailureKind::IterExhausted => "iter_exhausted",
            FailureKind::Stall => "stall",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_directive_legal_set_excludes_pre() {
        assert!(PreDirective::try_from_str("run").is_some());
        assert!(PreDirective::try_from_str("end").is_some());
        assert!(PreDirective::try_from_str("pre").is_none());
        assert!(PreDirective::try_from_str("halt").is_none());
    }

    #[test]
    fn post_directive_legal_set_covers_pre_run_end() {
        assert!(PostDirective::try_from_str("pre").is_some());
        assert!(PostDirective::try_from_str("run").is_some());
        assert!(PostDirective::try_from_str("end").is_some());
        assert!(PostDirective::try_from_str("halt").is_none());
    }

    #[test]
    fn phase_kind_str_round_trip() {
        assert_eq!(PhaseKind::Pre.as_str(), "pre");
        assert_eq!(PhaseKind::Run.as_str(), "run");
        assert_eq!(PhaseKind::Post.as_str(), "post");
    }

    #[test]
    fn failure_kind_stall_str_round_trip() {
        assert_eq!(FailureKind::Stall.as_str(), "stall");
    }
}
