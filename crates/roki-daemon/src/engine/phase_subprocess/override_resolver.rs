//! Per-phase override resolver: composes the catalog default with the two
//! mutually-exclusive override forms (`extension.phase.<name>.command` slash-
//! command swap; `prompt_template_<phase>` template body) plus the additive
//! scalar overrides (`max_turns`, `stall_seconds`, `max_attempts`).
//!
//! The both-forms refusal is enforced canonically in
//! [`crate::workflow::schema::validate`]; this resolver redundantly asserts
//! the invariant so a future code path that builds a `WorkflowPolicy` outside
//! the validator does not silently degrade behavior.
//!
//! Spec refs: requirements.md Req 6.7; design.md "PhaseSubprocessAdapter"
//! override flow.

use thiserror::Error;

use crate::engine::phase_subprocess::catalog::{
    CatalogError, PhaseCatalogEntry, PhaseName, catalog_default,
};
use crate::orchestrator::state::Mode;
use crate::workflow::schema::WorkflowPolicy;

/// Default per-phase stall budget when neither catalog nor override sets one.
pub const DEFAULT_PHASE_STALL_SECONDS: u32 = 120;
/// Default per-phase retry attempts when no override sets one.
pub const DEFAULT_PHASE_MAX_ATTEMPTS: u32 = 3;

/// Resolved invocation shape with effective scalar bounds applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedInvocation {
    /// Catalog default; scalar bounds reflect catalog + any operator scalar
    /// overrides on the phase.
    CatalogDefault {
        entry: PhaseCatalogEntry,
        max_turns: u32,
        stall_seconds: u32,
        max_attempts: u32,
    },
    /// `extension.phase.<name>.command` swap. Daemon launches
    /// `claude -p '<command>'`.
    SlashCommandOverride {
        command: String,
        max_turns: u32,
        stall_seconds: u32,
        max_attempts: u32,
    },
    /// `prompt_template_<phase>` body. Daemon launches
    /// `claude --input-format stream-json` and writes the rendered body to
    /// stdin.
    TemplateOverride {
        template_name: String,
        max_turns: u32,
        stall_seconds: u32,
        max_attempts: u32,
    },
}

/// Override-resolution failure modes.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum OverrideError {
    /// Catalog rejection (e.g., `Classify` outside `NeedsClassify`).
    #[error(transparent)]
    Catalog(#[from] CatalogError),

    /// Defense-in-depth: both override forms declared for the same phase.
    /// Canonical refusal lives in `workflow::schema::validate`; this branch
    /// fires only if a `WorkflowPolicy` was constructed without that
    /// validator (Req 6.7).
    #[error(
        "phase `{phase}` declares both `extension.phase.{phase}.command` and \
         `prompt_template_{phase}`; the two override forms are mutually \
         exclusive (Req 6.7)"
    )]
    BothOverrideForms { phase: String },
}

/// Borrows a [`WorkflowPolicy`] for the lifetime of a single resolution call.
pub struct OverrideResolver<'p> {
    policy: &'p WorkflowPolicy,
}

impl<'p> OverrideResolver<'p> {
    pub fn new(policy: &'p WorkflowPolicy) -> Self {
        Self { policy }
    }

    /// Resolve the effective invocation for a `(phase, mode)` pair. The
    /// catalog is consulted first because it owns mode-legality and the
    /// default `max_turns`; override forms then layer on top.
    pub fn resolve(
        &self,
        phase: PhaseName,
        mode: Mode,
    ) -> Result<ResolvedInvocation, OverrideError> {
        let catalog_entry = catalog_default(phase, mode)?;
        let phase_key = phase_key(phase);
        let phase_cfg = self.policy.phases.get(phase_key);

        let scalar_max_turns = phase_cfg
            .and_then(|cfg| cfg.max_turns)
            .unwrap_or(catalog_entry.default_max_turns);
        let scalar_stall = phase_cfg
            .and_then(|cfg| cfg.stall_seconds)
            .unwrap_or(DEFAULT_PHASE_STALL_SECONDS);
        let scalar_attempts = phase_cfg
            .and_then(|cfg| cfg.max_attempts)
            .unwrap_or(DEFAULT_PHASE_MAX_ATTEMPTS);

        let template_block_name = format!("prompt_template_{phase_key}");
        let has_template = self.policy.blocks.contains_key(&template_block_name);
        let command_override = phase_cfg.and_then(|cfg| cfg.command.as_deref());

        if command_override.is_some() && has_template {
            return Err(OverrideError::BothOverrideForms {
                phase: phase_key.to_owned(),
            });
        }

        if let Some(command) = command_override {
            return Ok(ResolvedInvocation::SlashCommandOverride {
                command: command.to_owned(),
                max_turns: scalar_max_turns,
                stall_seconds: scalar_stall,
                max_attempts: scalar_attempts,
            });
        }

        if has_template {
            return Ok(ResolvedInvocation::TemplateOverride {
                template_name: template_block_name,
                max_turns: scalar_max_turns,
                stall_seconds: scalar_stall,
                max_attempts: scalar_attempts,
            });
        }

        Ok(ResolvedInvocation::CatalogDefault {
            entry: catalog_entry,
            max_turns: scalar_max_turns,
            stall_seconds: scalar_stall,
            max_attempts: scalar_attempts,
        })
    }
}

/// Map a [`PhaseName`] to the snake-case key used in
/// `extension.phase.<name>` and `prompt_template_<name>`.
fn phase_key(phase: PhaseName) -> &'static str {
    match phase {
        PhaseName::Classify => "classify",
        PhaseName::Implement => "implement",
        PhaseName::Review => "review",
        PhaseName::Validate => "validate",
        PhaseName::OpenPr => "open_pr",
        PhaseName::CiFix => "ci_fix",
        PhaseName::FinalizeReview => "finalize_review",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use serde_json::Value;

    use crate::workflow::schema::{
        OrchestratorConfig, PhaseConfig, WorkflowPolicy,
    };

    fn empty_policy() -> WorkflowPolicy {
        WorkflowPolicy {
            orchestrator: OrchestratorConfig::default(),
            phases: BTreeMap::new(),
            server: Value::Object(Default::default()),
            blocks: BTreeMap::new(),
            raw_unknowns: Value::Object(Default::default()),
        }
    }

    fn policy_with(
        phases: BTreeMap<String, PhaseConfig>,
        blocks: BTreeMap<String, String>,
    ) -> WorkflowPolicy {
        let mut p = empty_policy();
        p.phases = phases;
        p.blocks = blocks;
        p
    }

    #[test]
    fn no_override_returns_catalog_default_with_default_scalars() {
        let policy = empty_policy();
        let resolver = OverrideResolver::new(&policy);
        let resolved = resolver
            .resolve(PhaseName::Implement, Mode::SpecDriven)
            .unwrap();
        match resolved {
            ResolvedInvocation::CatalogDefault {
                entry,
                max_turns,
                stall_seconds,
                max_attempts,
            } => {
                assert_eq!(max_turns, entry.default_max_turns);
                assert_eq!(max_turns, 50);
                assert_eq!(stall_seconds, DEFAULT_PHASE_STALL_SECONDS);
                assert_eq!(max_attempts, DEFAULT_PHASE_MAX_ATTEMPTS);
            }
            other => panic!("expected CatalogDefault, got {other:?}"),
        }
    }

    #[test]
    fn command_only_override_returns_slash_command_override() {
        let mut phases = BTreeMap::new();
        phases.insert(
            "review".to_owned(),
            PhaseConfig {
                command: Some("/custom-review".to_owned()),
                max_turns: Some(42),
                stall_seconds: Some(180),
                max_attempts: Some(5),
            },
        );
        let policy = policy_with(phases, BTreeMap::new());
        let resolver = OverrideResolver::new(&policy);
        match resolver.resolve(PhaseName::Review, Mode::SpecDriven).unwrap() {
            ResolvedInvocation::SlashCommandOverride {
                command,
                max_turns,
                stall_seconds,
                max_attempts,
            } => {
                assert_eq!(command, "/custom-review");
                assert_eq!(max_turns, 42);
                assert_eq!(stall_seconds, 180);
                assert_eq!(max_attempts, 5);
            }
            other => panic!("expected SlashCommandOverride, got {other:?}"),
        }
    }

    #[test]
    fn template_only_override_returns_template_override() {
        let mut blocks = BTreeMap::new();
        blocks.insert("prompt_template_review".to_owned(), "body".to_owned());
        let policy = policy_with(BTreeMap::new(), blocks);
        let resolver = OverrideResolver::new(&policy);
        match resolver.resolve(PhaseName::Review, Mode::SpecDriven).unwrap() {
            ResolvedInvocation::TemplateOverride {
                template_name,
                max_turns,
                stall_seconds,
                max_attempts,
            } => {
                assert_eq!(template_name, "prompt_template_review");
                // No scalar override: catalog default flows through.
                assert_eq!(max_turns, 30);
                assert_eq!(stall_seconds, DEFAULT_PHASE_STALL_SECONDS);
                assert_eq!(max_attempts, DEFAULT_PHASE_MAX_ATTEMPTS);
            }
            other => panic!("expected TemplateOverride, got {other:?}"),
        }
    }

    #[test]
    fn both_override_forms_for_same_phase_yields_refusal() {
        let mut phases = BTreeMap::new();
        phases.insert(
            "review".to_owned(),
            PhaseConfig {
                command: Some("/x".to_owned()),
                ..PhaseConfig::default()
            },
        );
        let mut blocks = BTreeMap::new();
        blocks.insert("prompt_template_review".to_owned(), "body".to_owned());
        let policy = policy_with(phases, blocks);
        let resolver = OverrideResolver::new(&policy);
        let err = resolver
            .resolve(PhaseName::Review, Mode::SpecDriven)
            .unwrap_err();
        assert_eq!(
            err,
            OverrideError::BothOverrideForms {
                phase: "review".to_owned()
            }
        );
    }

    #[test]
    fn scalar_override_layers_on_top_of_template_override() {
        let mut phases = BTreeMap::new();
        // Template selected for `validate` via the template block; scalars
        // declared via `extension.phase.validate.*` apply on top.
        phases.insert(
            "validate".to_owned(),
            PhaseConfig {
                command: None,
                max_turns: Some(7),
                stall_seconds: Some(90),
                max_attempts: Some(2),
            },
        );
        let mut blocks = BTreeMap::new();
        blocks.insert(
            "prompt_template_validate".to_owned(),
            "body".to_owned(),
        );
        let policy = policy_with(phases, blocks);
        let resolver = OverrideResolver::new(&policy);
        match resolver
            .resolve(PhaseName::Validate, Mode::SpecDriven)
            .unwrap()
        {
            ResolvedInvocation::TemplateOverride {
                template_name,
                max_turns,
                stall_seconds,
                max_attempts,
            } => {
                assert_eq!(template_name, "prompt_template_validate");
                assert_eq!(max_turns, 7);
                assert_eq!(stall_seconds, 90);
                assert_eq!(max_attempts, 2);
            }
            other => panic!("expected TemplateOverride, got {other:?}"),
        }
    }

    #[test]
    fn catalog_mode_illegality_propagates_through_resolver() {
        let policy = empty_policy();
        let resolver = OverrideResolver::new(&policy);
        // Classify is illegal outside NeedsClassify per catalog table.
        let err = resolver
            .resolve(PhaseName::Classify, Mode::SpecDriven)
            .unwrap_err();
        assert!(matches!(err, OverrideError::Catalog(_)));
    }

    #[test]
    fn scalar_max_turns_override_applies_with_catalog_default_invocation() {
        let mut phases = BTreeMap::new();
        phases.insert(
            "implement".to_owned(),
            PhaseConfig {
                command: None,
                max_turns: Some(99),
                stall_seconds: None,
                max_attempts: None,
            },
        );
        let policy = policy_with(phases, BTreeMap::new());
        let resolver = OverrideResolver::new(&policy);
        match resolver
            .resolve(PhaseName::Implement, Mode::SpecDriven)
            .unwrap()
        {
            ResolvedInvocation::CatalogDefault {
                max_turns,
                stall_seconds,
                max_attempts,
                ..
            } => {
                assert_eq!(max_turns, 99);
                assert_eq!(stall_seconds, DEFAULT_PHASE_STALL_SECONDS);
                assert_eq!(max_attempts, DEFAULT_PHASE_MAX_ATTEMPTS);
            }
            other => panic!("expected CatalogDefault, got {other:?}"),
        }
    }
}
