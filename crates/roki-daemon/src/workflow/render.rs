//! Orchestrator + per-phase prompt rendering.
//!
//! Two render contexts: [`OrchestratorRenderContext`] (one per orchestrator
//! launch) and [`PhaseRenderContext`] (one per phase nomination). Each carries
//! exactly the variables the loader's contract surfaces to template authors:
//! issue id, title, body, mode, and either the orchestrator's bucketed
//! lifecycle state or the phase's optional target spec / worktree path /
//! `additional_context` blob.
//!
//! On Liquid render failure the orchestrator path falls back to a
//! deterministic plain-text prompt that still includes issue id, title,
//! body, and mode (Req 6.6). Phase rendering surfaces the failure to the
//! caller, which is responsible for the engine adapter's recovery policy.
//!
//! The phase render contract reserves a stable, machine-extractable
//! delimiter pair for the orchestrator's `additional_context` so phase
//! subprocesses can locate it deterministically regardless of operator
//! template formatting:
//!
//! ```text
//! <!-- ROKI:ADDITIONAL_CONTEXT BEGIN -->
//! <verbatim additional_context bytes>
//! <!-- ROKI:ADDITIONAL_CONTEXT END -->
//! ```
//!
//! Spec refs: requirements.md Req 6.6, Req 13.4.

use std::path::PathBuf;

use thiserror::Error;

use crate::orchestrator::state::{IssueId, Mode};

/// Marker block surrounding `additional_context` in a rendered phase prompt.
/// Stable forwarding contract per Req 13.4 — engine adapters that consume the
/// rendered prompt downstream may parse this delimiter pair as an exact
/// string match.
pub const ADDITIONAL_CONTEXT_BEGIN: &str = "<!-- ROKI:ADDITIONAL_CONTEXT BEGIN -->";
/// Closing marker; see [`ADDITIONAL_CONTEXT_BEGIN`].
pub const ADDITIONAL_CONTEXT_END: &str = "<!-- ROKI:ADDITIONAL_CONTEXT END -->";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RenderError {
    /// Liquid template parse / render failure.
    #[error("Liquid render error: {0}")]
    Liquid(String),
}

/// Variables made available to the orchestrator system-prompt template.
#[derive(Debug, Clone)]
pub struct OrchestratorRenderContext {
    pub issue: IssueId,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub mode: Mode,
    pub bucketed_state: String,
}

/// Variables made available to a per-phase prompt template (when the
/// operator overrides the catalog default with a `prompt_template_<phase>`
/// block, or when the phase is one of the four daemon-internal Liquid-template
/// phases).
#[derive(Debug, Clone)]
pub struct PhaseRenderContext {
    pub issue: IssueId,
    pub target_spec: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub mode: Mode,
    pub additional_context: Option<String>,
}

/// Render the orchestrator system prompt against `template`. Returns
/// [`RenderError::Liquid`] on any parse / render failure; the caller logs
/// the failure and falls back to [`fallback_orchestrator_prompt`].
pub fn render_orchestrator_prompt(
    template: &str,
    ctx: &OrchestratorRenderContext,
) -> Result<String, RenderError> {
    let parser = liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(|e| RenderError::Liquid(e.to_string()))?;
    let parsed = parser
        .parse(template)
        .map_err(|e| RenderError::Liquid(e.to_string()))?;
    let mut globals = liquid::Object::new();
    globals.insert(
        "issue".into(),
        liquid::model::Value::scalar(ctx.issue.to_string()),
    );
    globals.insert("title".into(), liquid::model::Value::scalar(ctx.title.clone()));
    globals.insert("body".into(), liquid::model::Value::scalar(ctx.body.clone()));
    globals.insert(
        "labels".into(),
        liquid::model::Value::Array(
            ctx.labels
                .iter()
                .map(|l| liquid::model::Value::scalar(l.clone()))
                .collect(),
        ),
    );
    globals.insert(
        "mode".into(),
        liquid::model::Value::scalar(mode_token(ctx.mode).to_owned()),
    );
    globals.insert(
        "bucketed_state".into(),
        liquid::model::Value::scalar(ctx.bucketed_state.clone()),
    );

    parsed
        .render(&globals)
        .map_err(|e| RenderError::Liquid(e.to_string()))
}

/// Deterministic plain-text fallback when [`render_orchestrator_prompt`]
/// fails. Always non-empty; always names issue id, title, body, and mode so
/// the orchestrator session has the minimum context to act.
pub fn fallback_orchestrator_prompt(ctx: &OrchestratorRenderContext) -> String {
    format!(
        "Roki orchestrator session (fallback prompt — operator template render failed).\n\
         \n\
         Issue: {issue}\n\
         Title: {title}\n\
         Mode: {mode}\n\
         \n\
         Body:\n{body}\n",
        issue = ctx.issue,
        title = ctx.title,
        mode = mode_token(ctx.mode),
        body = ctx.body,
    )
}

/// Render a per-phase prompt against `template`. The orchestrator's
/// `additional_context` (when present) is wrapped between the
/// [`ADDITIONAL_CONTEXT_BEGIN`] / [`ADDITIONAL_CONTEXT_END`] markers and
/// exposed to the template as the `additional_context` Liquid variable so
/// authors can place it deterministically. If the template author does not
/// reference `additional_context`, the wrapped block is appended to the
/// rendered output to preserve the engine adapter's stable forwarding
/// contract.
pub fn render_phase_prompt(template: &str, ctx: &PhaseRenderContext) -> Result<String, RenderError> {
    let parser = liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(|e| RenderError::Liquid(e.to_string()))?;
    let parsed = parser
        .parse(template)
        .map_err(|e| RenderError::Liquid(e.to_string()))?;

    let wrapped_context = ctx.additional_context.as_ref().map(|raw| {
        format!("{ADDITIONAL_CONTEXT_BEGIN}\n{raw}\n{ADDITIONAL_CONTEXT_END}")
    });

    let mut globals = liquid::Object::new();
    globals.insert(
        "issue".into(),
        liquid::model::Value::scalar(ctx.issue.to_string()),
    );
    globals.insert(
        "mode".into(),
        liquid::model::Value::scalar(mode_token(ctx.mode).to_owned()),
    );
    globals.insert(
        "target_spec".into(),
        ctx.target_spec
            .as_ref()
            .map(|s| liquid::model::Value::scalar(s.clone()))
            .unwrap_or(liquid::model::Value::Nil),
    );
    globals.insert(
        "worktree_path".into(),
        ctx.worktree_path
            .as_ref()
            .map(|p| liquid::model::Value::scalar(p.display().to_string()))
            .unwrap_or(liquid::model::Value::Nil),
    );
    globals.insert(
        "additional_context".into(),
        wrapped_context
            .as_ref()
            .map(|s| liquid::model::Value::scalar(s.clone()))
            .unwrap_or(liquid::model::Value::Nil),
    );

    let mut rendered = parsed
        .render(&globals)
        .map_err(|e| RenderError::Liquid(e.to_string()))?;

    if let Some(wrapped) = wrapped_context.as_ref() {
        // Stable forwarding contract: the wrapped block must appear in the
        // output verbatim. If the template author already placed the
        // delimiters, do not duplicate.
        let already_present = rendered.contains(ADDITIONAL_CONTEXT_BEGIN)
            && rendered.contains(ADDITIONAL_CONTEXT_END);
        if !already_present {
            if !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            rendered.push_str(wrapped);
            rendered.push('\n');
        }
    }

    Ok(rendered)
}

fn mode_token(mode: Mode) -> &'static str {
    match mode {
        Mode::SpecDriven => "SPEC_DRIVEN",
        Mode::NeedsClassify => "NEEDS_CLASSIFY",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orch_ctx() -> OrchestratorRenderContext {
        OrchestratorRenderContext {
            issue: IssueId::from("ENG-42"),
            title: "Implement widget".to_owned(),
            body: "Acceptance: must do widget things.".to_owned(),
            labels: vec!["roki:ready".to_owned()],
            mode: Mode::SpecDriven,
            bucketed_state: "Pending".to_owned(),
        }
    }

    #[test]
    fn orchestrator_render_substitutes_mode() {
        let template = "issue {{ issue }} mode={{ mode }} state={{ bucketed_state }}";
        let rendered = render_orchestrator_prompt(template, &orch_ctx()).unwrap();
        assert!(rendered.contains("ENG-42"), "issue id missing: {rendered}");
        assert!(rendered.contains("SPEC_DRIVEN"), "mode missing: {rendered}");
        assert!(rendered.contains("Pending"));
    }

    #[test]
    fn orchestrator_render_failure_falls_back_with_required_context() {
        // Liquid syntax error: unbalanced tag.
        let bad_template = "{% if true %}unterminated";
        let err = render_orchestrator_prompt(bad_template, &orch_ctx()).unwrap_err();
        assert!(matches!(err, RenderError::Liquid(_)));

        let fallback = fallback_orchestrator_prompt(&orch_ctx());
        assert!(fallback.contains("ENG-42"), "fallback missing issue: {fallback}");
        assert!(
            fallback.contains("Implement widget"),
            "fallback missing title: {fallback}",
        );
        assert!(
            fallback.contains("Acceptance: must do widget things."),
            "fallback missing body: {fallback}",
        );
        assert!(fallback.contains("SPEC_DRIVEN"), "fallback missing mode: {fallback}");
    }

    #[test]
    fn phase_render_passes_additional_context_through_documented_delimiters() {
        let template = "Phase prompt for {{ issue }}\n";
        let ctx = PhaseRenderContext {
            issue: IssueId::from("ENG-9"),
            target_spec: Some("foo-spec".to_owned()),
            worktree_path: Some(PathBuf::from("/tmp/wt")),
            mode: Mode::NeedsClassify,
            additional_context: Some("retry: reviewer found XYZ".to_owned()),
        };
        let rendered = render_phase_prompt(template, &ctx).unwrap();

        let begin_idx = rendered
            .find(ADDITIONAL_CONTEXT_BEGIN)
            .expect("begin marker must appear");
        let end_idx = rendered
            .find(ADDITIONAL_CONTEXT_END)
            .expect("end marker must appear");
        assert!(begin_idx < end_idx, "begin must precede end");
        let between = &rendered[begin_idx + ADDITIONAL_CONTEXT_BEGIN.len()..end_idx];
        assert!(
            between.contains("retry: reviewer found XYZ"),
            "additional_context body missing between markers: {between:?}",
        );
    }

    #[test]
    fn phase_render_template_can_place_additional_context_explicitly() {
        // When the template author references the variable, the wrapped
        // block appears in-place and is not re-appended at the tail.
        let template = "header\n{{ additional_context }}\nfooter\n";
        let ctx = PhaseRenderContext {
            issue: IssueId::from("ENG-1"),
            target_spec: None,
            worktree_path: None,
            mode: Mode::SpecDriven,
            additional_context: Some("hello".to_owned()),
        };
        let rendered = render_phase_prompt(template, &ctx).unwrap();
        let begin_count = rendered.matches(ADDITIONAL_CONTEXT_BEGIN).count();
        let end_count = rendered.matches(ADDITIONAL_CONTEXT_END).count();
        assert_eq!(begin_count, 1, "delimiters must not be duplicated");
        assert_eq!(end_count, 1);
        assert!(rendered.contains("hello"));
    }

    #[test]
    fn phase_render_without_additional_context_omits_delimiters() {
        let template = "issue={{ issue }} mode={{ mode }}";
        let ctx = PhaseRenderContext {
            issue: IssueId::from("ENG-7"),
            target_spec: None,
            worktree_path: None,
            mode: Mode::SpecDriven,
            additional_context: None,
        };
        let rendered = render_phase_prompt(template, &ctx).unwrap();
        assert!(!rendered.contains(ADDITIONAL_CONTEXT_BEGIN));
        assert!(rendered.contains("ENG-7"));
        assert!(rendered.contains("SPEC_DRIVEN"));
    }
}
