//! Phase catalog: canonical `(phase, mode)` -> default invocation table.
//!
//! `PhaseName` is canonically declared here and re-exported from
//! `engine::orchestrator_session::action_parser`. Per design.md the daemon
//! resolves the default invocation per phase × mode and applies per-phase
//! overrides on top.
//!
//! Spec refs: requirements.md Req 5.6, 5.12; design.md "Components and
//! Interfaces" PhaseSubprocessAdapter table (lines ~782-792).

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::orchestrator::state::{IssueId, Mode};
use crate::workflow::schema::WorkflowPolicy;

/// Canonical seven-phase enum. Serializes as kebab-case (`open_pr`,
/// `ci_fix`, `finalize_review`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseName {
    Classify,
    Implement,
    Review,
    Validate,
    OpenPr,
    CiFix,
    FinalizeReview,
}

/// Cheap-clone shared handle to the parsed `WORKFLOW.md` policy. Backed by
/// [`Arc`] so the watcher can hot-swap the underlying policy without
/// invalidating in-flight phase launch contexts (a phase keeps the policy
/// it spawned with for its lifetime per Req 6.3).
pub type WorkflowPolicyHandle = Arc<WorkflowPolicy>;

/// Stub for the resolved permission strategy (`--settings` vs
/// `--dangerously-skip-permissions`). Real type lands in tasks 7.x.
#[derive(Debug, Clone, Default)]
pub struct PermissionStrategy;

/// Captured at the point the orchestrator nominates a phase. Carries every
/// piece of context the phase subprocess needs to spawn.
#[derive(Debug, Clone)]
pub struct PhaseLaunchContext {
    pub issue: IssueId,
    pub phase: PhaseName,
    pub mode: Mode,
    pub additional_context: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub session_tempdir: PathBuf,
    pub max_turns: u32,
    pub workflow_policy: WorkflowPolicyHandle,
    pub permission_strategy: PermissionStrategy,
    pub allowed_tools: Vec<String>,
}

/// Two shapes the catalog can produce: a slash-command invocation against an
/// installed Claude Code skill, or a daemon-internal rendered template body
/// piped on stdin to `claude --input-format stream-json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseInvocation {
    SlashCommand {
        skill: String,
        arg_template: String,
    },
    DaemonInternalTemplate {
        template_name: String,
    },
}

/// One row in the documented `(phase, mode) -> default invocation` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseCatalogEntry {
    pub invocation: PhaseInvocation,
    pub default_max_turns: u32,
}

/// Catalog lookup failure modes.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CatalogError {
    /// `(phase, mode)` is not a legal pairing per design.md (e.g., `classify`
    /// outside `NeedsClassify`).
    #[error("phase {phase:?} is not legal in mode {mode:?}")]
    ModeIllegal { phase: PhaseName, mode: Mode },
}

/// Resolve the documented default invocation for a `(phase, mode)` pair. The
/// per-phase override surface is applied in a separate resolver layer.
pub fn catalog_default(
    phase: PhaseName,
    mode: Mode,
) -> Result<PhaseCatalogEntry, CatalogError> {
    use Mode::*;
    use PhaseName::*;

    let entry = match (phase, mode) {
        (Classify, NeedsClassify) => PhaseCatalogEntry {
            invocation: PhaseInvocation::SlashCommand {
                skill: "roki-classify".to_owned(),
                arg_template: "<ticket-context>".to_owned(),
            },
            default_max_turns: 5,
        },
        // Classify is mode-illegal in SPEC_DRIVEN.
        (Classify, _) => return Err(CatalogError::ModeIllegal { phase, mode }),

        (Implement, SpecDriven) => PhaseCatalogEntry {
            invocation: PhaseInvocation::SlashCommand {
                skill: "kiro-impl".to_owned(),
                arg_template: "<target>".to_owned(),
            },
            default_max_turns: 50,
        },
        (Implement, NeedsClassify) => PhaseCatalogEntry {
            invocation: PhaseInvocation::DaemonInternalTemplate {
                template_name: "prompt_template_implement_direct".to_owned(),
            },
            default_max_turns: 50,
        },

        (Review, _) => PhaseCatalogEntry {
            invocation: PhaseInvocation::SlashCommand {
                skill: "kiro-review".to_owned(),
                arg_template: "<target>".to_owned(),
            },
            default_max_turns: 30,
        },

        (Validate, SpecDriven) => PhaseCatalogEntry {
            invocation: PhaseInvocation::SlashCommand {
                skill: "kiro-validate-impl".to_owned(),
                arg_template: "<target>".to_owned(),
            },
            default_max_turns: 20,
        },
        (Validate, NeedsClassify) => PhaseCatalogEntry {
            invocation: PhaseInvocation::DaemonInternalTemplate {
                template_name: "prompt_template_validate_direct".to_owned(),
            },
            default_max_turns: 20,
        },

        (OpenPr, _) => PhaseCatalogEntry {
            invocation: PhaseInvocation::DaemonInternalTemplate {
                template_name: "prompt_template_open_pr".to_owned(),
            },
            default_max_turns: 10,
        },

        (CiFix, _) => PhaseCatalogEntry {
            invocation: PhaseInvocation::SlashCommand {
                skill: "roki-ci-fix".to_owned(),
                arg_template: "<context>".to_owned(),
            },
            default_max_turns: 30,
        },

        (FinalizeReview, _) => PhaseCatalogEntry {
            invocation: PhaseInvocation::SlashCommand {
                skill: "roki-finalize-review".to_owned(),
                arg_template: "<target>".to_owned(),
            },
            default_max_turns: 20,
        },
    };
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slash(skill: &str, arg: &str) -> PhaseInvocation {
        PhaseInvocation::SlashCommand {
            skill: skill.to_owned(),
            arg_template: arg.to_owned(),
        }
    }

    fn template(name: &str) -> PhaseInvocation {
        PhaseInvocation::DaemonInternalTemplate {
            template_name: name.to_owned(),
        }
    }

    #[test]
    fn classify_only_legal_in_needs_classify() {
        let entry = catalog_default(PhaseName::Classify, Mode::NeedsClassify).unwrap();
        assert_eq!(entry.invocation, slash("roki-classify", "<ticket-context>"));
        assert_eq!(entry.default_max_turns, 5);

        let err = catalog_default(PhaseName::Classify, Mode::SpecDriven).unwrap_err();
        assert_eq!(
            err,
            CatalogError::ModeIllegal {
                phase: PhaseName::Classify,
                mode: Mode::SpecDriven
            }
        );
    }

    #[test]
    fn implement_spec_driven_uses_kiro_impl_slash_command() {
        let entry = catalog_default(PhaseName::Implement, Mode::SpecDriven).unwrap();
        assert_eq!(entry.invocation, slash("kiro-impl", "<target>"));
        assert_eq!(entry.default_max_turns, 50);
    }

    #[test]
    fn implement_needs_classify_uses_internal_template() {
        let entry = catalog_default(PhaseName::Implement, Mode::NeedsClassify).unwrap();
        assert_eq!(entry.invocation, template("prompt_template_implement_direct"));
        assert_eq!(entry.default_max_turns, 50);
    }

    #[test]
    fn review_uses_kiro_review_in_both_modes() {
        for mode in [Mode::SpecDriven, Mode::NeedsClassify] {
            let entry = catalog_default(PhaseName::Review, mode).unwrap();
            assert_eq!(entry.invocation, slash("kiro-review", "<target>"));
            assert_eq!(entry.default_max_turns, 30);
        }
    }

    #[test]
    fn validate_branches_on_mode() {
        let spec = catalog_default(PhaseName::Validate, Mode::SpecDriven).unwrap();
        assert_eq!(spec.invocation, slash("kiro-validate-impl", "<target>"));
        assert_eq!(spec.default_max_turns, 20);

        let direct = catalog_default(PhaseName::Validate, Mode::NeedsClassify).unwrap();
        assert_eq!(direct.invocation, template("prompt_template_validate_direct"));
        assert_eq!(direct.default_max_turns, 20);
    }

    #[test]
    fn open_pr_uses_internal_template_in_both_modes() {
        for mode in [Mode::SpecDriven, Mode::NeedsClassify] {
            let entry = catalog_default(PhaseName::OpenPr, mode).unwrap();
            assert_eq!(entry.invocation, template("prompt_template_open_pr"));
            assert_eq!(entry.default_max_turns, 10);
        }
    }

    #[test]
    fn ci_fix_uses_slash_command_in_both_modes() {
        for mode in [Mode::SpecDriven, Mode::NeedsClassify] {
            let entry = catalog_default(PhaseName::CiFix, mode).unwrap();
            assert_eq!(entry.invocation, slash("roki-ci-fix", "<context>"));
            assert_eq!(entry.default_max_turns, 30);
        }
    }

    #[test]
    fn finalize_review_uses_slash_command_in_both_modes() {
        for mode in [Mode::SpecDriven, Mode::NeedsClassify] {
            let entry = catalog_default(PhaseName::FinalizeReview, mode).unwrap();
            assert_eq!(entry.invocation, slash("roki-finalize-review", "<target>"));
            assert_eq!(entry.default_max_turns, 20);
        }
    }

    #[test]
    fn phase_name_serde_uses_snake_case() {
        // serde rename_all = snake_case yields the documented kebab-style identifiers
        // (`open_pr`, `ci_fix`, `finalize_review`).
        assert_eq!(
            serde_json::to_string(&PhaseName::OpenPr).unwrap(),
            "\"open_pr\""
        );
        assert_eq!(
            serde_json::to_string(&PhaseName::CiFix).unwrap(),
            "\"ci_fix\""
        );
        assert_eq!(
            serde_json::to_string(&PhaseName::FinalizeReview).unwrap(),
            "\"finalize_review\""
        );
    }
}
