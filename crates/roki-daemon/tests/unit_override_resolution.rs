//! Integration smoke tests for the per-phase override resolver.
//!
//! In-file unit tests in
//! `engine::phase_subprocess::override_resolver::tests` cover the resolver
//! against synthetic policies. Here we drive the public surface through the
//! four documented combinations of override forms and assert each lands on
//! the documented `ResolvedInvocation` variant or refusal.

use std::collections::BTreeMap;

use roki_daemon::engine::phase_subprocess::catalog::PhaseName;
use roki_daemon::engine::phase_subprocess::override_resolver::{
    DEFAULT_PHASE_MAX_ATTEMPTS, DEFAULT_PHASE_STALL_SECONDS, OverrideError, OverrideResolver,
    ResolvedInvocation,
};
use roki_daemon::orchestrator::state::Mode;
use roki_daemon::workflow::schema::{OrchestratorConfig, PhaseConfig, WorkflowPolicy};

fn empty_policy() -> WorkflowPolicy {
    WorkflowPolicy {
        orchestrator: OrchestratorConfig::default(),
        phases: BTreeMap::new(),
        server: serde_json::Value::Object(Default::default()),
        blocks: BTreeMap::new(),
        raw_unknowns: serde_json::Value::Object(Default::default()),
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
fn neither_form_declared_returns_catalog_default() {
    let policy = empty_policy();
    let resolver = OverrideResolver::new(&policy);
    match resolver
        .resolve(PhaseName::Implement, Mode::SpecDriven)
        .expect("catalog default resolution")
    {
        ResolvedInvocation::CatalogDefault {
            entry,
            max_turns,
            stall_seconds,
            max_attempts,
        } => {
            assert_eq!(max_turns, entry.default_max_turns);
            assert_eq!(stall_seconds, DEFAULT_PHASE_STALL_SECONDS);
            assert_eq!(max_attempts, DEFAULT_PHASE_MAX_ATTEMPTS);
        }
        other => panic!("expected CatalogDefault, got {other:?}"),
    }
}

#[test]
fn command_only_returns_slash_command_override() {
    let mut phases = BTreeMap::new();
    phases.insert(
        "review".to_owned(),
        PhaseConfig {
            command: Some("/custom-review".to_owned()),
            max_turns: Some(11),
            stall_seconds: Some(77),
            max_attempts: Some(4),
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
            assert_eq!(max_turns, 11);
            assert_eq!(stall_seconds, 77);
            assert_eq!(max_attempts, 4);
        }
        other => panic!("expected SlashCommandOverride, got {other:?}"),
    }
}

#[test]
fn template_only_returns_template_override() {
    let mut blocks = BTreeMap::new();
    blocks.insert(
        "prompt_template_review".to_owned(),
        "custom review body".to_owned(),
    );
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
            // No scalar overrides: catalog default flows through.
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
