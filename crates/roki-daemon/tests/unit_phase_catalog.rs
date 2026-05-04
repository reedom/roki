//! Integration smoke tests for the phase catalog default table.
//!
//! In-file unit tests in `engine::phase_subprocess::catalog::tests` assert
//! every individual `(phase, mode)` row; this file walks the documented
//! 13-row matrix parametrically and pins the mode-illegality refusal for
//! Classify outside NeedsClassify.

use roki_daemon::engine::phase_subprocess::catalog::{
    CatalogError, PhaseInvocation, PhaseName, catalog_default,
};
use roki_daemon::orchestrator::state::Mode;

fn skill_name(invocation: &PhaseInvocation) -> Option<&str> {
    match invocation {
        PhaseInvocation::SlashCommand { skill, .. } => Some(skill.as_str()),
        PhaseInvocation::DaemonInternalTemplate { .. } => None,
    }
}

fn template_name(invocation: &PhaseInvocation) -> Option<&str> {
    match invocation {
        PhaseInvocation::DaemonInternalTemplate { template_name } => Some(template_name.as_str()),
        PhaseInvocation::SlashCommand { .. } => None,
    }
}

#[test]
fn every_documented_pair_resolves_to_the_canonical_invocation() {
    // (phase, mode, expected_kind, expected_max_turns)
    // expected_kind = Some("kiro-impl") for SlashCommand or
    //                 None means "expect template" — checked separately.
    type Row = (PhaseName, Mode, Option<&'static str>, Option<&'static str>, u32);
    let rows: Vec<Row> = vec![
        (PhaseName::Classify, Mode::NeedsClassify, Some("roki-classify"), None, 5),
        (PhaseName::Implement, Mode::SpecDriven, Some("kiro-impl"), None, 50),
        (
            PhaseName::Implement,
            Mode::NeedsClassify,
            None,
            Some("prompt_template_implement_direct"),
            50,
        ),
        (PhaseName::Review, Mode::SpecDriven, Some("kiro-review"), None, 30),
        (PhaseName::Review, Mode::NeedsClassify, Some("kiro-review"), None, 30),
        (
            PhaseName::Validate,
            Mode::SpecDriven,
            Some("kiro-validate-impl"),
            None,
            20,
        ),
        (
            PhaseName::Validate,
            Mode::NeedsClassify,
            None,
            Some("prompt_template_validate_direct"),
            20,
        ),
        (
            PhaseName::OpenPr,
            Mode::SpecDriven,
            None,
            Some("prompt_template_open_pr"),
            10,
        ),
        (
            PhaseName::OpenPr,
            Mode::NeedsClassify,
            None,
            Some("prompt_template_open_pr"),
            10,
        ),
        (PhaseName::CiFix, Mode::SpecDriven, Some("roki-ci-fix"), None, 30),
        (PhaseName::CiFix, Mode::NeedsClassify, Some("roki-ci-fix"), None, 30),
        (
            PhaseName::FinalizeReview,
            Mode::SpecDriven,
            Some("roki-finalize-review"),
            None,
            20,
        ),
        (
            PhaseName::FinalizeReview,
            Mode::NeedsClassify,
            Some("roki-finalize-review"),
            None,
            20,
        ),
    ];

    for (phase, mode, expected_skill, expected_template, max_turns) in rows {
        let entry = catalog_default(phase, mode)
            .unwrap_or_else(|err| panic!("({phase:?}, {mode:?}) must resolve: {err}"));
        assert_eq!(
            entry.default_max_turns, max_turns,
            "default_max_turns for ({phase:?}, {mode:?})"
        );
        assert_eq!(skill_name(&entry.invocation), expected_skill);
        assert_eq!(template_name(&entry.invocation), expected_template);
    }
}

#[test]
fn classify_outside_needs_classify_is_refused() {
    let err = catalog_default(PhaseName::Classify, Mode::SpecDriven).unwrap_err();
    assert_eq!(
        err,
        CatalogError::ModeIllegal {
            phase: PhaseName::Classify,
            mode: Mode::SpecDriven,
        }
    );
}

#[test]
fn classify_inside_needs_classify_uses_roki_classify_with_max_turns_5() {
    let entry = catalog_default(PhaseName::Classify, Mode::NeedsClassify).unwrap();
    assert_eq!(skill_name(&entry.invocation), Some("roki-classify"));
    assert_eq!(entry.default_max_turns, 5);
}
