//! Phase subprocess adapter.
//!
//! Spawns one bounded `claude` subprocess per `action=run_phase` directive,
//! resolving `(phase, mode)` against the catalog/override layer, applying
//! permissions (with the Classify pin), and rendering the per-phase context
//! envelope through [`crate::workflow::render::render_phase_prompt`] so the
//! orchestrator's `additional_context` flows through the documented
//! delimiter contract verbatim (Req 13.4).
//!
//! The adapter intentionally exposes a free-standing
//! [`build_invocation`] function in addition to [`PhaseSubprocessAdapter::spawn`]
//! so unit tests can assert the resolved args/env/stdin payload for every
//! `(phase, mode, override)` row without spawning a real subprocess.
//!
//! Spec refs: requirements.md Req 4.4, 5.6, 5.12, 6.7, 7.1, 9.1, 9.2, 9.3,
//! 9.4, 11.5, 13.4; design.md "PhaseSubprocessAdapter".

use std::collections::BTreeMap;
use std::path::PathBuf;

use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::engine::claude::{ClaudeBinary, ClaudeError};
use crate::engine::phase_subprocess::catalog::{
    PhaseInvocation, PhaseLaunchContext, PhaseName,
};
use crate::engine::phase_subprocess::override_resolver::{
    OverrideError, OverrideResolver, ResolvedInvocation,
};
use crate::engine::stream::{StreamLine, parse_line};
use crate::logging::{PerIssueDebugSink, RoleTag, StreamTag};
use crate::permissions::{
    PermissionConfigError, PermissionResolver, PermissionStrategy, ResolvedPermission, Sandbox,
    classify_allowed_tools,
};
use crate::workflow::render::{PhaseRenderContext, RenderError, render_phase_prompt};

/// Channel buffer for stream-json events drained off the phase subprocess.
const STREAM_CHANNEL_CAPACITY: usize = 64;
/// Channel buffer for stderr lines drained off the phase subprocess.
const STDERR_CHANNEL_CAPACITY: usize = 32;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Errors surfaced from [`PhaseSubprocessAdapter::spawn`] and
/// [`build_invocation`].
#[derive(Debug, Error)]
pub enum AdapterError {
    #[error(transparent)]
    Override(#[from] OverrideError),

    #[error(transparent)]
    Permission(#[from] PermissionConfigError),

    #[error("phase {phase:?} requires a worktree but PhaseLaunchContext.worktree_path is None")]
    MissingWorktree { phase: PhaseName },

    #[error("classify phase must NOT carry a worktree path; got {path}")]
    ClassifyHasWorktree { path: PathBuf },

    #[error("template `{template_name}` not found in workflow policy blocks")]
    TemplateNotFound { template_name: String },

    #[error(transparent)]
    Render(#[from] RenderError),

    #[error("failed to spawn phase subprocess: {0}")]
    Spawn(#[source] ClaudeError),

    #[error("failed to write rendered prompt to phase subprocess stdin: {source}")]
    WriteStdin {
        #[source]
        source: std::io::Error,
    },
}

/// Stable, fully-resolved invocation shape ready to feed into
/// [`crate::engine::claude::ClaudeBinary::spawn_builder`]. Returned by
/// [`build_invocation`] so unit tests can assert the resolved args/env/cwd
/// without actually spawning a child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// CLI arguments passed to `claude`, in order. Does NOT include the
    /// `--settings <path>` pair the spawn primitive prepends; that is held
    /// separately in [`Invocation::settings_path`] so the test can assert
    /// each surface independently.
    pub args: Vec<String>,
    /// Optional `--settings` JSON path to be prepended at spawn time when
    /// the resolved permission strategy is
    /// [`PermissionStrategy::SettingsAllowlist`].
    pub settings_path: Option<PathBuf>,
    /// Environment variables to set on the child.
    pub env: BTreeMap<String, String>,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// When `Some`, the rendered Liquid prompt body to be written to the
    /// child's stdin (template-style invocation). `None` for slash-command
    /// invocations whose prompt is carried inline as `claude -p '<command>'`.
    pub stdin_payload: Option<String>,
    /// Final allowlist applied via `--settings`. `None` when the strategy is
    /// `--dangerously-skip-permissions`.
    pub allowed_tools: Option<Vec<String>>,
    /// Resolved permission descriptor (sandbox / reject_elicitations /
    /// strategy) for callers that need to log the launch decision.
    pub permission: ResolvedPermission,
    /// Effective `--max-turns` value applied to this launch.
    pub max_turns: u32,
    /// Effective per-phase stall window in seconds.
    pub stall_seconds: u32,
}

/// Owned handle over a spawned phase subprocess.
#[derive(Debug)]
pub struct PhaseProcessHandle {
    pub child: tokio::process::Child,
    /// Typed stream-json events parsed off the child's stdout. Closes when
    /// the child's stdout closes.
    pub stream_rx: mpsc::Receiver<StreamLine>,
    /// Stderr lines, forwarded verbatim. Each line is also emitted as a
    /// `tracing::warn!` event tagged `role=phase:<phase-name>`.
    pub stderr_rx: mpsc::Receiver<String>,
    /// Effective per-phase stall window (Req 5.7) for callers that drive
    /// the stall detector outside the adapter.
    pub stall_seconds: u32,
    /// Effective `--max-turns` applied to this launch.
    pub max_turns: u32,
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
}

/// Per-launch dependencies the orchestrator core hands the adapter.
#[derive(Debug, Clone)]
pub struct PhaseSubprocessAdapter {
    binary: ClaudeBinary,
    permissions: PermissionResolver,
}

impl PhaseSubprocessAdapter {
    pub fn new(binary: ClaudeBinary, permissions: PermissionResolver) -> Self {
        Self { binary, permissions }
    }

    /// Resolve the `(phase, mode)` invocation, render the per-phase prompt
    /// envelope, and spawn the bounded `claude` subprocess.
    pub async fn spawn(
        &self,
        ctx: PhaseLaunchContext,
        debug_sink: Option<PerIssueDebugSink>,
    ) -> Result<PhaseProcessHandle, AdapterError> {
        let invocation = build_invocation(&ctx, &self.permissions)?;

        let role_tag = RoleTag::Phase(phase_key(ctx.phase).to_owned());

        let mut spawn = self.binary.clone().spawn_builder();
        if let Some(path) = invocation.settings_path.clone() {
            spawn = spawn.with_settings(path);
        }
        spawn = spawn.args(invocation.args.clone()).cwd(invocation.cwd.clone());
        for (k, v) in &invocation.env {
            spawn = spawn.env(k.clone(), v.clone());
        }

        let mut process = spawn.spawn().await.map_err(AdapterError::Spawn)?;

        if let Some(payload) = invocation.stdin_payload.as_ref() {
            process
                .stdin
                .write_all(payload.as_bytes())
                .await
                .map_err(|source| AdapterError::WriteStdin { source })?;
            // The fake_claude harness expects a final newline so its
            // `read_line` unblocks. Real claude tolerates the explicit close.
            if !payload.ends_with('\n') {
                process
                    .stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|source| AdapterError::WriteStdin { source })?;
            }
            process
                .stdin
                .flush()
                .await
                .map_err(|source| AdapterError::WriteStdin { source })?;
        }
        // For slash-command invocations the prompt is inlined via `-p`, so
        // there is nothing to push down stdin: drop it to signal EOF early.
        drop(process.stdin);

        let crate::engine::claude::ClaudeProcess {
            child,
            stdin: _,
            stdout,
            stderr,
        } = process;

        let (stream_tx, stream_rx) = mpsc::channel::<StreamLine>(STREAM_CHANNEL_CAPACITY);
        let (stderr_tx, stderr_rx) = mpsc::channel::<String>(STDERR_CHANNEL_CAPACITY);

        // Share the debug sink across stdout + stderr drainers via a tokio
        // mutex so the per-issue file captures both streams in arrival order.
        let debug = debug_sink
            .map(|sink| std::sync::Arc::new(tokio::sync::Mutex::new(sink)));

        let stdout_task = tokio::spawn(stdout_drainer(
            stdout,
            stream_tx,
            role_tag.clone(),
            debug.clone(),
        ));
        let stderr_task = tokio::spawn(stderr_drainer(
            stderr,
            stderr_tx,
            role_tag,
            debug,
        ));

        Ok(PhaseProcessHandle {
            child,
            stream_rx,
            stderr_rx,
            stall_seconds: invocation.stall_seconds,
            max_turns: invocation.max_turns,
            stdout_task: Some(stdout_task),
            stderr_task: Some(stderr_task),
        })
    }
}

impl PhaseProcessHandle {
    /// Await both IO drainer tasks. Useful in tests; production callers
    /// typically take the receivers and drive shutdown via the child handle.
    pub async fn join_io_tasks(&mut self) {
        if let Some(t) = self.stdout_task.take() {
            let _ = t.await;
        }
        if let Some(t) = self.stderr_task.take() {
            let _ = t.await;
        }
    }
}

// ---------------------------------------------------------------------------
// build_invocation: resolves args/env/stdin without spawning
// ---------------------------------------------------------------------------

/// Resolve the full invocation shape for a `(phase, mode, override)` tuple.
/// Pure: no I/O, no spawn — the spawn primitive is the only place that
/// touches the OS.
pub fn build_invocation(
    ctx: &PhaseLaunchContext,
    permissions: &PermissionResolver,
) -> Result<Invocation, AdapterError> {
    // Worktree invariant: classify must NOT have a worktree (it predates
    // the feature dir); every other phase must HAVE one.
    match (ctx.phase, &ctx.worktree_path) {
        (PhaseName::Classify, Some(path)) => {
            return Err(AdapterError::ClassifyHasWorktree { path: path.clone() });
        }
        (phase, None) if !matches!(phase, PhaseName::Classify) => {
            return Err(AdapterError::MissingWorktree { phase });
        }
        _ => {}
    }

    let resolver = OverrideResolver::new(&ctx.workflow_policy);
    let resolved = resolver.resolve(ctx.phase, ctx.mode)?;

    let mut permission = permissions.resolve_for_phase(ctx.phase)?;
    // Classify-phase pin (Req 5.12, 7.1, 9.4): regardless of whether the
    // operator's broader phase-subprocess sandbox is wider, the classify
    // surface is always `{Read,Glob,Grep}` + ReadOnly + reject elicitations.
    if matches!(ctx.phase, PhaseName::Classify) {
        permission.allowed_tools = Some(classify_allowed_tools());
        permission.sandbox = Sandbox::ReadOnly;
        permission.reject_elicitations = true;
    }

    let (args, stdin_payload) = match &resolved {
        ResolvedInvocation::CatalogDefault { entry, max_turns, .. } => {
            match &entry.invocation {
                PhaseInvocation::SlashCommand { skill, arg_template } => {
                    let command = format!("/{skill} {arg_template}");
                    (
                        slash_command_args(&command, *max_turns, false),
                        None,
                    )
                }
                PhaseInvocation::DaemonInternalTemplate { template_name } => {
                    let body = render_template_body(template_name, ctx)?;
                    (template_args(*max_turns), Some(body))
                }
            }
        }
        ResolvedInvocation::SlashCommandOverride { command, max_turns, .. } => {
            (slash_command_args(command, *max_turns, true), None)
        }
        ResolvedInvocation::TemplateOverride { template_name, max_turns, .. } => {
            let body = render_template_body(template_name, ctx)?;
            (template_args(*max_turns), Some(body))
        }
    };

    let max_turns = resolved_max_turns(&resolved);
    let stall_seconds = resolved_stall_seconds(&resolved);
    let allowed_tools = permission.allowed_tools.clone();
    let settings_path = match &permission.strategy {
        PermissionStrategy::SettingsAllowlist { settings_path } => Some(settings_path.clone()),
        PermissionStrategy::DangerouslySkipPermissions => None,
    };

    // Append the strategy-specific flag. `--dangerously-skip-permissions`
    // replaces `--settings`; the spawn primitive prepends `--settings` only
    // when settings_path is Some.
    let mut final_args = args;
    if matches!(
        permission.strategy,
        PermissionStrategy::DangerouslySkipPermissions
    ) {
        final_args.push("--dangerously-skip-permissions".to_owned());
    }

    Ok(Invocation {
        args: final_args,
        settings_path,
        env: BTreeMap::new(),
        cwd: ctx.session_tempdir.clone(),
        stdin_payload,
        allowed_tools,
        permission,
        max_turns,
        stall_seconds,
    })
}

fn slash_command_args(command: &str, max_turns: u32, command_is_already_full: bool) -> Vec<String> {
    // For catalog defaults the skill+arg_template come pre-joined as
    // `/skill <arg>`. Override `command` strings are passed verbatim.
    let _ = command_is_already_full;
    vec![
        "-p".to_owned(),
        command.to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--max-turns".to_owned(),
        max_turns.to_string(),
    ]
}

fn template_args(max_turns: u32) -> Vec<String> {
    vec![
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--max-turns".to_owned(),
        max_turns.to_string(),
    ]
}

fn render_template_body(
    template_name: &str,
    ctx: &PhaseLaunchContext,
) -> Result<String, AdapterError> {
    let template = ctx
        .workflow_policy
        .blocks
        .get(template_name)
        .ok_or_else(|| AdapterError::TemplateNotFound {
            template_name: template_name.to_owned(),
        })?;

    let render_ctx = PhaseRenderContext {
        issue: ctx.issue.clone(),
        target_spec: None,
        worktree_path: ctx.worktree_path.clone(),
        mode: ctx.mode,
        additional_context: ctx.additional_context.clone(),
    };
    Ok(render_phase_prompt(template, &render_ctx)?)
}

fn resolved_max_turns(resolved: &ResolvedInvocation) -> u32 {
    match resolved {
        ResolvedInvocation::CatalogDefault { max_turns, .. }
        | ResolvedInvocation::SlashCommandOverride { max_turns, .. }
        | ResolvedInvocation::TemplateOverride { max_turns, .. } => *max_turns,
    }
}

fn resolved_stall_seconds(resolved: &ResolvedInvocation) -> u32 {
    match resolved {
        ResolvedInvocation::CatalogDefault { stall_seconds, .. }
        | ResolvedInvocation::SlashCommandOverride { stall_seconds, .. }
        | ResolvedInvocation::TemplateOverride { stall_seconds, .. } => *stall_seconds,
    }
}

/// Phase key used in operator config and structured logs. Mirrors the
/// `phase_key` helper in `override_resolver` but kept private here so the
/// caller surface stays minimal.
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

// ---------------------------------------------------------------------------
// IO drainers
// ---------------------------------------------------------------------------

type SharedDebugSink = std::sync::Arc<tokio::sync::Mutex<PerIssueDebugSink>>;

async fn stdout_drainer(
    mut stdout: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    stream_tx: mpsc::Sender<StreamLine>,
    role: RoleTag,
    debug: Option<SharedDebugSink>,
) {
    while let Ok(Some(line)) = stdout.next_line().await {
        if let Some(sink) = &debug {
            sink.lock().await.append(StreamTag::Stdout, &role, &line);
        }
        match parse_line(&line) {
            Ok(parsed) => {
                if stream_tx.send(parsed).await.is_err() {
                    return;
                }
            }
            Err(err) => {
                // Malformed stream-json: log structurally; downstream
                // observers still see the raw line via the debug sink.
                warn!(role = %role, error = %err, line = %line, "phase stream-json parse error");
            }
        }
    }
}

async fn stderr_drainer(
    mut stderr: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStderr>>,
    stderr_tx: mpsc::Sender<String>,
    role: RoleTag,
    debug: Option<SharedDebugSink>,
) {
    while let Ok(Some(line)) = stderr.next_line().await {
        warn!(role = %role, stderr = %line, "phase stderr");
        if let Some(sink) = &debug {
            sink.lock().await.append(StreamTag::Stderr, &role, &line);
        }
        if stderr_tx.send(line).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::phase_subprocess::catalog::WorkflowPolicyHandle;
    use crate::orchestrator::state::{IssueId, Mode};
    use crate::workflow::schema::{
        OrchestratorConfig, PhaseConfig, WorkflowPolicy,
    };
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn baseline_policy() -> WorkflowPolicy {
        let mut blocks = BTreeMap::new();
        blocks.insert(
            "prompt_template_implement_direct".to_owned(),
            "Implement {{ issue }} mode={{ mode }}\n".to_owned(),
        );
        blocks.insert(
            "prompt_template_validate_direct".to_owned(),
            "Validate {{ issue }} mode={{ mode }}\n".to_owned(),
        );
        blocks.insert(
            "prompt_template_open_pr".to_owned(),
            "OpenPR {{ issue }} mode={{ mode }}\n".to_owned(),
        );
        WorkflowPolicy {
            orchestrator: OrchestratorConfig::default(),
            phases: BTreeMap::new(),
            server: Value::Object(Default::default()),
            blocks,
            raw_unknowns: Value::Object(Default::default()),
        }
    }

    fn handle(policy: WorkflowPolicy) -> WorkflowPolicyHandle {
        Arc::new(policy)
    }

    fn ctx_for(
        phase: PhaseName,
        mode: Mode,
        worktree: Option<PathBuf>,
        policy: WorkflowPolicyHandle,
        additional: Option<&str>,
    ) -> PhaseLaunchContext {
        PhaseLaunchContext {
            issue: IssueId::from("ENG-7"),
            phase,
            mode,
            additional_context: additional.map(str::to_owned),
            worktree_path: worktree,
            session_tempdir: PathBuf::from("/tmp/session"),
            max_turns: 0,
            workflow_policy: policy,
            permission_strategy: PermissionStrategy::SettingsAllowlist {
                settings_path: PathBuf::from("/tmp/settings.json"),
            },
            allowed_tools: vec!["Read".to_owned(), "Bash".to_owned()],
        }
    }

    fn permissions() -> PermissionResolver {
        PermissionResolver::with_settings_path(
            PathBuf::from("/tmp/settings.json"),
            vec!["Read".to_owned(), "Bash".to_owned(), "Edit".to_owned()],
        )
    }

    #[test]
    fn classify_invocation_uses_p_with_max_turns_5_and_pinned_allowlist() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Classify,
            Mode::NeedsClassify,
            None,
            policy,
            Some("retry: prior classification rejected"),
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        assert!(inv.args.iter().any(|a| a == "-p"));
        let p_idx = inv.args.iter().position(|a| a == "-p").unwrap();
        assert!(inv.args[p_idx + 1].starts_with("/roki-classify"));
        let mt_idx = inv.args.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(inv.args[mt_idx + 1], "5");

        let allowed = inv.allowed_tools.expect("classify pins an allowlist");
        assert_eq!(
            allowed,
            vec!["Read".to_owned(), "Glob".to_owned(), "Grep".to_owned()]
        );
        assert_eq!(inv.permission.sandbox, Sandbox::ReadOnly);
        assert!(inv.permission.reject_elicitations);
        // Classify never feeds stdin: prompt is inlined via `-p`.
        assert!(inv.stdin_payload.is_none());
    }

    #[test]
    fn classify_with_worktree_path_is_rejected() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Classify,
            Mode::NeedsClassify,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let err = build_invocation(&ctx, &permissions()).unwrap_err();
        assert!(matches!(err, AdapterError::ClassifyHasWorktree { .. }));
    }

    #[test]
    fn non_classify_without_worktree_is_rejected() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Implement,
            Mode::SpecDriven,
            None,
            policy,
            None,
        );
        let err = build_invocation(&ctx, &permissions()).unwrap_err();
        match err {
            AdapterError::MissingWorktree { phase } => {
                assert_eq!(phase, PhaseName::Implement);
            }
            other => panic!("expected MissingWorktree, got {other:?}"),
        }
    }

    #[test]
    fn implement_spec_driven_uses_kiro_impl_slash_command_with_max_turns_50() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Implement,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        let p_idx = inv.args.iter().position(|a| a == "-p").unwrap();
        assert!(inv.args[p_idx + 1].starts_with("/kiro-impl"));
        let mt_idx = inv.args.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(inv.args[mt_idx + 1], "50");
        assert!(inv.stdin_payload.is_none());
        assert_eq!(inv.permission.sandbox, Sandbox::WorkspaceWrite);
    }

    #[test]
    fn implement_needs_classify_renders_template_to_stdin() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Implement,
            Mode::NeedsClassify,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            Some("reviewer found XYZ"),
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        // Template form: --input-format stream-json + stdin payload.
        assert!(inv.args.iter().any(|a| a == "--input-format"));
        let body = inv.stdin_payload.expect("template form delivers stdin");
        assert!(body.contains("ENG-7"));
        assert!(body.contains("reviewer found XYZ"));
        // Verbatim delimiter forwarding contract per Req 13.4.
        assert!(body.contains("ROKI:ADDITIONAL_CONTEXT BEGIN"));
        assert!(body.contains("ROKI:ADDITIONAL_CONTEXT END"));
    }

    #[test]
    fn open_pr_renders_template_in_both_modes() {
        for mode in [Mode::SpecDriven, Mode::NeedsClassify] {
            let policy = handle(baseline_policy());
            let ctx = ctx_for(
                PhaseName::OpenPr,
                mode,
                Some(PathBuf::from("/wt/eng-7")),
                policy,
                None,
            );
            let inv = build_invocation(&ctx, &permissions()).unwrap();
            assert!(inv.args.iter().any(|a| a == "--input-format"));
            assert!(inv.stdin_payload.is_some(), "open_pr feeds template stdin");
            let mt_idx = inv.args.iter().position(|a| a == "--max-turns").unwrap();
            assert_eq!(inv.args[mt_idx + 1], "10");
        }
    }

    #[test]
    fn slash_command_override_replaces_catalog_default_command() {
        let mut p = baseline_policy();
        p.phases.insert(
            "review".to_owned(),
            PhaseConfig {
                command: Some("/custom-review --target foo".to_owned()),
                max_turns: Some(42),
                ..PhaseConfig::default()
            },
        );
        let policy = handle(p);
        let ctx = ctx_for(
            PhaseName::Review,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        let p_idx = inv.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(inv.args[p_idx + 1], "/custom-review --target foo");
        let mt_idx = inv.args.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(inv.args[mt_idx + 1], "42");
    }

    #[test]
    fn template_override_for_review_renders_to_stdin() {
        let mut p = baseline_policy();
        p.blocks.insert(
            "prompt_template_review".to_owned(),
            "Custom review for {{ issue }}\n".to_owned(),
        );
        let policy = handle(p);
        let ctx = ctx_for(
            PhaseName::Review,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        assert!(inv.args.iter().any(|a| a == "--input-format"));
        let body = inv.stdin_payload.expect("template override delivers stdin");
        assert!(body.contains("Custom review for ENG-7"));
    }

    #[test]
    fn settings_path_is_passed_for_settings_strategy() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Implement,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        assert_eq!(
            inv.settings_path,
            Some(PathBuf::from("/tmp/settings.json"))
        );
        assert!(!inv.args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn dangerously_skip_overrides_settings_path_for_non_classify() {
        let perms = permissions().with_dangerously_skip_override(true);
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Implement,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &perms).unwrap();
        assert!(inv.args.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert_eq!(inv.settings_path, None);
        assert!(inv.allowed_tools.is_none());
    }

    #[test]
    fn classify_pins_allowlist_even_when_operator_set_dangerous_skip() {
        // Per Req 5.12: Classify is unconditionally pinned to {Read,Glob,Grep}.
        let perms = permissions().with_dangerously_skip_override(true);
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Classify,
            Mode::NeedsClassify,
            None,
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &perms).unwrap();
        assert_eq!(
            inv.allowed_tools.as_deref(),
            Some(
                [
                    "Read".to_owned(),
                    "Glob".to_owned(),
                    "Grep".to_owned()
                ]
                .as_slice()
            ),
        );
        assert_eq!(inv.permission.sandbox, Sandbox::ReadOnly);
        assert!(inv.permission.reject_elicitations);
    }

    #[test]
    fn finalize_review_uses_slash_command_with_default_max_turns_20() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::FinalizeReview,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        let p_idx = inv.args.iter().position(|a| a == "-p").unwrap();
        assert!(inv.args[p_idx + 1].starts_with("/roki-finalize-review"));
        let mt_idx = inv.args.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(inv.args[mt_idx + 1], "20");
    }

    #[test]
    fn ci_fix_uses_slash_command_default_max_turns_30() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::CiFix,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        let p_idx = inv.args.iter().position(|a| a == "-p").unwrap();
        assert!(inv.args[p_idx + 1].starts_with("/roki-ci-fix"));
        let mt_idx = inv.args.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(inv.args[mt_idx + 1], "30");
    }

    #[test]
    fn validate_branches_on_mode() {
        let policy = handle(baseline_policy());
        let spec_ctx = ctx_for(
            PhaseName::Validate,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy.clone(),
            None,
        );
        let spec_inv = build_invocation(&spec_ctx, &permissions()).unwrap();
        let p_idx = spec_inv.args.iter().position(|a| a == "-p").unwrap();
        assert!(spec_inv.args[p_idx + 1].starts_with("/kiro-validate-impl"));

        let nc_ctx = ctx_for(
            PhaseName::Validate,
            Mode::NeedsClassify,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let nc_inv = build_invocation(&nc_ctx, &permissions()).unwrap();
        assert!(nc_inv.stdin_payload.is_some());
        assert!(nc_inv.args.iter().any(|a| a == "--input-format"));
    }

    #[test]
    fn additional_context_flows_through_documented_delimiter_for_template_form() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::OpenPr,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            Some("verbatim ADDITIONAL CONTEXT body"),
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        let body = inv.stdin_payload.expect("template form delivers stdin");
        assert!(
            body.contains("verbatim ADDITIONAL CONTEXT body"),
            "additional_context body must appear verbatim in rendered prompt: {body}",
        );
        // Per Req 13.4: delimiter contract is stable.
        assert!(body.contains("<!-- ROKI:ADDITIONAL_CONTEXT BEGIN -->"));
        assert!(body.contains("<!-- ROKI:ADDITIONAL_CONTEXT END -->"));
    }

    #[test]
    fn cwd_is_session_tempdir() {
        let policy = handle(baseline_policy());
        let ctx = ctx_for(
            PhaseName::Implement,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let inv = build_invocation(&ctx, &permissions()).unwrap();
        assert_eq!(inv.cwd, PathBuf::from("/tmp/session"));
    }

    #[test]
    fn template_missing_block_is_actionable_error() {
        let mut p = baseline_policy();
        p.blocks.remove("prompt_template_open_pr");
        let policy = handle(p);
        let ctx = ctx_for(
            PhaseName::OpenPr,
            Mode::SpecDriven,
            Some(PathBuf::from("/wt/eng-7")),
            policy,
            None,
        );
        let err = build_invocation(&ctx, &permissions()).unwrap_err();
        match err {
            AdapterError::TemplateNotFound { template_name } => {
                assert_eq!(template_name, "prompt_template_open_pr");
            }
            other => panic!("expected TemplateNotFound, got {other:?}"),
        }
    }

    // ----- End-to-end fake_claude integration tests -----

    #[cfg(unix)]
    fn fake_claude_path() -> std::path::PathBuf {
        // Build the example binary on demand, sharing the workspace target
        // dir so the orchestrator-session adapter and this adapter reuse the
        // same artifact.
        let status = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "--example",
                "fake_claude",
                "-p",
                "roki-daemon",
            ])
            .status()
            .expect("invoke cargo build --example fake_claude");
        assert!(status.success(), "fake_claude example build failed");
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .ancestors()
            .nth(2)
            .expect("workspace root is two levels above the daemon manifest")
            .to_path_buf();
        workspace_root
            .join("target")
            .join("debug")
            .join("examples")
            .join("fake_claude")
    }

    #[cfg(unix)]
    fn write_fake_mode(dir: &std::path::Path, mode: &str) {
        std::fs::write(dir.join(".fake_claude_mode"), mode).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn end_to_end_spawn_phase_success_emits_terminal_result_line() {
        let tmp = tempfile::tempdir().unwrap();
        write_fake_mode(tmp.path(), "phase_success");

        let bin = ClaudeBinary::discover(Some(&fake_claude_path())).unwrap();
        let perms = PermissionResolver::with_settings_path(
            tmp.path().join("settings.json"),
            vec!["Read".to_owned()],
        );
        // Write a stub settings file so `--settings <path>` does not bomb
        // even though fake_claude ignores the flag.
        std::fs::write(tmp.path().join("settings.json"), b"{}").unwrap();

        let adapter = PhaseSubprocessAdapter::new(bin, perms);

        let mut blocks = std::collections::BTreeMap::new();
        blocks.insert(
            "prompt_template_open_pr".to_owned(),
            "open_pr {{ issue }}\n".to_owned(),
        );
        let policy = WorkflowPolicy {
            orchestrator: OrchestratorConfig::default(),
            phases: BTreeMap::new(),
            server: serde_json::Value::Object(Default::default()),
            blocks,
            raw_unknowns: serde_json::Value::Object(Default::default()),
        };

        let ctx = PhaseLaunchContext {
            issue: IssueId::from("ENG-9"),
            phase: PhaseName::OpenPr,
            mode: Mode::SpecDriven,
            additional_context: Some("verbatim ctx body".to_owned()),
            worktree_path: Some(tmp.path().join("wt")),
            session_tempdir: tmp.path().to_path_buf(),
            max_turns: 0,
            workflow_policy: handle(policy),
            permission_strategy: PermissionStrategy::SettingsAllowlist {
                settings_path: tmp.path().join("settings.json"),
            },
            allowed_tools: vec!["Read".to_owned()],
        };

        let mut handle = adapter.spawn(ctx, None).await.expect("phase spawn");
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(5), handle.stream_rx.recv())
                .await
                .expect("stream rx timeout")
                .expect("stream rx closed");
        match event {
            crate::engine::stream::StreamLine::Result { subtype, .. } => {
                assert_eq!(subtype, "success");
            }
            other => panic!("expected Result, got {other:?}"),
        }
        let status = handle.child.wait().await.expect("child wait");
        assert!(status.success());
        handle.join_io_tasks().await;
    }
}
