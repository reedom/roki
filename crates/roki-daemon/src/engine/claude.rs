//! Claude Code subprocess supervisor.
//!
//! Task 2.10 of the roki-mvp spec. Wires the stream-json line parser
//! ([`crate::engine::stream`], task 2.7), the engine policy controller
//! ([`crate::engine::policy`], task 2.8), and the resolved permission strategy
//! ([`crate::permissions`], task 2.9) into a single supervised lifecycle that
//! launches `claude --print --output-format stream-json` per active issue and
//! drives the bounded-loop semantics from requirements.md §5 and design.md
//! "Engine".
//!
//! The supervisor produces, for every successful launch, exactly one terminal
//! [`SupervisedEvent::Exited`] event whose payload is a [`WorkerOutcome`]
//! distinguishing the four observable terminations:
//!
//! * [`WorkerOutcome::CleanExit`]              — exit status 0 (req 5.5).
//! * [`WorkerOutcome::NonCleanExit { code }`]  — non-zero exit status (req 5.6).
//! * [`WorkerOutcome::TurnBudgetExhausted`]    — turn budget hit (req 5.4).
//! * [`WorkerOutcome::Stalled { reason }`]     — event-inactivity beyond the
//!   configured stall window (req 5.3); the subprocess is killed before the
//!   terminal event is emitted.
//!
//! The agent prompt is delivered through a stable, machine-extractable
//! "prelude envelope" prepended to the session input on stdin (req 13.4):
//!
//! ```text
//! <<<ROKI_PRELUDE>>>
//! { ... JSON object with `version`, `tools`, `additional_context`, ... }
//! <<<END_PRELUDE>>>
//! <prompt>
//! ```
//!
//! When [`WorkerContext::additional_context`] is `Some(value)`, the value is
//! placed verbatim under the `additional_context` key inside the JSON object —
//! the MVP supervisor itself never interprets the contents (req 13.4); the
//! sentinel markers exist so that downstream specs (notably roki-review-gate)
//! can locate the envelope deterministically without depending on a JSON
//! parser at the agent end.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::time;

use crate::engine::policy::{EnginePolicy, StallReason, WorkerOutcome};
use crate::engine::stream::{EngineLifecycleEvent, parse_line};
use crate::orchestrator::state::{CorrelationId, IssueId, RepoId};
use crate::permissions::{PermissionMode, ResolvedPermission};
use crate::tools::{Registry, ToolDescriptor, ToolError};

/// Stable opening sentinel of the prelude envelope (req 13.4). Documented as
/// part of the daemon ↔ agent contract so downstream specs can locate the
/// envelope without re-parsing the surrounding prompt.
pub const PRELUDE_OPEN: &str = "<<<ROKI_PRELUDE>>>";

/// Stable closing sentinel of the prelude envelope (req 13.4).
pub const PRELUDE_CLOSE: &str = "<<<END_PRELUDE>>>";

/// JSON-key under which `WorkerContext::additional_context` is forwarded
/// verbatim inside the prelude envelope (req 13.4). Documented as a stable
/// contract so downstream specs (e.g. roki-review-gate's `.review-findings.json`)
/// can rely on it without coupling to MVP types.
pub const PRELUDE_ADDITIONAL_CONTEXT_KEY: &str = "additional_context";

/// JSON-key under which the worker tool catalog is forwarded inside the
/// prelude envelope. Stable for the same reason as
/// [`PRELUDE_ADDITIONAL_CONTEXT_KEY`].
pub const PRELUDE_TOOLS_KEY: &str = "tools";

/// Default polling cadence the stall watchdog uses to compare wall-clock time
/// against the most recent observed engine event.
const STALL_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Per-launch context handed to [`ClaudeEngineAdapter::launch`].
///
/// Mirrors the design.md §Engine `WorkerContext` shape with the additive
/// optional `additional_context` field reserved by Requirement 13.4. Field
/// names are part of the daemon ↔ agent contract: every additive optional
/// field flows through the same prelude-forwarding mechanism without
/// reinterpretation.
#[derive(Debug, Clone)]
pub struct WorkerContext {
    pub repo: RepoId,
    pub issue: IssueId,
    pub correlation_id: CorrelationId,
    pub workspace_dir: PathBuf,
    /// Rendered prompt the agent receives after the prelude envelope.
    pub prompt: String,
    /// Snapshot of the daemon's tool registry forwarded to the worker
    /// (req 7.1, 7.5).
    pub tool_catalog: Vec<ToolDescriptor>,
    /// Resolved permission strategy from [`crate::permissions`] (req 9.x).
    pub permission: ResolvedPermission,
    /// Engine policy knobs (turn budget, stall window, backoff growth).
    pub policy: EnginePolicy,
    /// Additive optional field reserved for downstream specs (req 13.4). The
    /// MVP supervisor forwards the value verbatim through the prelude
    /// envelope and does not interpret the contents.
    pub additional_context: Option<serde_json::Value>,
}

/// Stable JSON shape of the prelude payload (the body that lives between the
/// `<<<ROKI_PRELUDE>>>` / `<<<END_PRELUDE>>>` sentinels).
///
/// The struct is deliberately additive: future fields default to absent so an
/// older agent reading a newer envelope sees only the keys it knows. The
/// `version` field exists so downstream specs can detect the envelope shape
/// without sniffing for keys.
#[derive(Debug, Clone, Serialize)]
struct PreludePayload<'a> {
    /// Schema version of the prelude envelope.
    version: u32,
    /// Repo and issue identifiers (purely contextual; the MVP agent does not
    /// rely on these but downstream specs may correlate logs).
    repo: &'a str,
    issue: &'a str,
    /// Tool catalog snapshot.
    #[serde(rename = "tools")]
    tools: &'a [ToolDescriptor],
    /// Additive context from `WorkerContext::additional_context` (req 13.4).
    /// Skipped when `None` so the JSON shape stays minimal in the common
    /// case.
    #[serde(rename = "additional_context", skip_serializing_if = "Option::is_none")]
    additional_context: Option<&'a serde_json::Value>,
}

/// Schema version of the prelude envelope. Bumped only on a breaking shape
/// change (req 13.4 documents the forwarding mechanism as additive).
const PRELUDE_VERSION: u32 = 1;

/// Map a [`ToolError`] to a stable, redaction-safe discriminant for tracing.
///
/// We log the variant name rather than the rendered error string so the
/// supervisor's observability path stays robust even when a future tool
/// implementation forgets to redact a daemon-owned credential before
/// constructing the error (req 7.4).
fn error_kind(err: &ToolError) -> &'static str {
    match err {
        ToolError::MultipleOperations => "MULTIPLE_OPERATIONS",
        ToolError::InvalidInput { .. } => "INVALID_INPUT",
        ToolError::RateLimited { .. } => "RATE_LIMITED",
        ToolError::LinearHttpError { .. } => "LINEAR_HTTP_ERROR",
        ToolError::Network { .. } => "LINEAR_HTTP_ERROR",
        ToolError::RedactionFailed => "REDACTION_FAILED",
        ToolError::DuplicateName { .. } => "DUPLICATE_TOOL",
        ToolError::UnknownTool { .. } => "UNKNOWN_TOOL",
        ToolError::RegistryPoisoned => "REGISTRY_POISONED",
    }
}

/// Build the full session input the supervisor pipes to `claude --print`'s
/// stdin: the prelude envelope followed by the rendered prompt.
///
/// Pure function so unit tests can drive it without a live subprocess; the
/// supervisor's role is just to write the result onto the child's stdin.
pub fn build_session_input(ctx: &WorkerContext) -> String {
    let payload = PreludePayload {
        version: PRELUDE_VERSION,
        repo: ctx.repo.as_str(),
        issue: ctx.issue.as_str(),
        tools: &ctx.tool_catalog,
        additional_context: ctx.additional_context.as_ref(),
    };
    // serde_json::to_string never fails for our owned value types; fall back
    // to an empty object literal in the unreachable failure path so the
    // supervisor still produces a valid envelope rather than panicking.
    let body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned());
    format!(
        "{open}\n{body}\n{close}\n{prompt}",
        open = PRELUDE_OPEN,
        body = body,
        close = PRELUDE_CLOSE,
        prompt = ctx.prompt,
    )
}

/// Supervised event emitted while a `claude` subprocess is running.
///
/// The lifecycle stream is intentionally a flat sum of the per-line events
/// produced by [`crate::engine::stream::parse_line`] and a single terminal
/// outcome variant emitted by the supervisor when the process exits or is
/// killed by the policy controller. Exactly one [`SupervisedEvent::Exited`]
/// event is emitted per launch (design.md §Engine "one terminal `Exited`
/// event always emitted").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisedEvent {
    Lifecycle(EngineLifecycleEvent),
    Exited(WorkerOutcome),
}

/// Errors raised by the supervisor before any lifecycle events are emitted.
/// Once a launch reaches the streaming phase, every termination — including
/// I/O failures while reading stdout — is reported through the terminal
/// [`SupervisedEvent::Exited`] event so the orchestrator never has to handle
/// two failure shapes.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// `tokio::process::Command::spawn` failed before the child was created.
    #[error("failed to spawn claude binary `{binary}`: {source}")]
    Spawn {
        binary: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// stdin/stdout were not piped — programmer error in adapter wiring.
    #[error("claude subprocess pipe missing: {which}")]
    MissingPipe { which: &'static str },
}

/// Subprocess supervisor.
///
/// `binary` defaults to the literal `"claude"` so production callers pick up
/// the operator's `$PATH`. Tests inject a path to the fake binary that drives
/// the observable-completion matrix.
///
/// The optional `registry` field carries the daemon's tool registry (task 3.4).
/// When attached, every successful launch composes the worker's tool catalog
/// from [`Registry::catalog`] and dispatches agent tool calls through
/// [`ClaudeEngineAdapter::dispatch_tool`] so the daemon-owned credentials
/// (notably the Linear API token) never cross the subprocess boundary
/// (req 7.1, 7.2, 7.4).
#[derive(Clone)]
pub struct ClaudeEngineAdapter {
    binary: PathBuf,
    registry: Option<Arc<dyn Registry>>,
}

impl std::fmt::Debug for ClaudeEngineAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeEngineAdapter")
            .field("binary", &self.binary)
            .field("registry", &self.registry.as_ref().map(|_| "<registry>"))
            .finish()
    }
}

impl Default for ClaudeEngineAdapter {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            registry: None,
        }
    }
}

impl ClaudeEngineAdapter {
    /// Build an adapter that resolves `claude` from `$PATH`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an adapter that invokes the binary at `binary`. Used by the
    /// integration test harness with a fake `claude` binary.
    pub fn with_binary(binary: PathBuf) -> Self {
        Self {
            binary,
            registry: None,
        }
    }

    /// Attach a tool [`Registry`] used to compose the worker's tool catalog
    /// at launch and to dispatch agent-issued tool calls (task 3.4,
    /// req 7.1, 7.2, 7.4).
    ///
    /// The adapter takes a shared `Arc` so the orchestrator and the adapter
    /// observe a single source of truth for registered tools.
    pub fn with_registry(mut self, registry: Arc<dyn Registry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Forward an agent-issued tool call through the attached [`Registry`].
    ///
    /// Errors returned from this method are already redaction-safe: each
    /// registered tool (e.g. [`crate::tools::linear_graphql::LinearGraphqlTool`])
    /// scrubs daemon-owned credentials from any error message it emits before
    /// the value reaches the [`ToolError`] enum (req 7.4). The supervisor
    /// itself never copies tool input or output into log messages — only the
    /// tool name is recorded — so the daemon-owned token cannot leak through
    /// the observability path either.
    pub async fn dispatch_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| ToolError::UnknownTool {
                name: name.to_string(),
            })?;
        // Trace the dispatch with only the tool name; never log input or
        // output bytes because they may contain credentials or PII.
        tracing::debug!(
            target: "engine.claude.tools",
            tool = name,
            "dispatching agent tool call through registry",
        );
        let result = registry.dispatch(name, input).await;
        if let Err(err) = &result {
            // The error string from `LinearGraphqlTool` is already redacted;
            // log the variant discriminant rather than the rendered message
            // to keep this path robust against future tools that forget to
            // redact internally.
            tracing::warn!(
                target: "engine.claude.tools",
                tool = name,
                error_kind = error_kind(err),
                "tool dispatch returned an error",
            );
        }
        result
    }

    /// Launch a supervised `claude` session and stream lifecycle events into
    /// `events`. Resolves to the terminal [`WorkerOutcome`] after emitting
    /// exactly one [`SupervisedEvent::Exited`] (req 5.x, design.md §Engine).
    ///
    /// On a `LaunchError` (spawn failure) no events are emitted; the caller
    /// is expected to surface the error directly through its own
    /// observability path.
    pub async fn launch(
        &self,
        mut ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError> {
        // Task 3.4 / req 7.1: when a registry is attached and the caller did
        // not pre-populate the catalog, compose it from the live registry so
        // every spawned worker subprocess sees the daemon's audited tool
        // surface.
        if ctx.tool_catalog.is_empty()
            && let Some(registry) = &self.registry
        {
            ctx.tool_catalog = registry.catalog();
        }

        let session_input = build_session_input(&ctx);

        let mut command = Command::new(&self.binary);
        // Documented base flags. `--bare` is intentionally omitted so that
        // kiro-* skills under `~/.claude/skills/` remain discoverable
        // (req 5.7).
        command.arg("--print");
        command.arg("--output-format");
        command.arg("stream-json");

        match &ctx.permission.mode {
            // Req 9.3: forward the operator-resolved allowlist to the worker.
            PermissionMode::Allowlist { settings_path } => {
                command.arg("--settings");
                command.arg(settings_path);
            }
            // Req 9.4: pass the elevated-permission flag (the warn log is
            // emitted by the permission resolver, not the adapter).
            PermissionMode::DangerousFallback => {
                command.arg("--dangerously-skip-permissions");
            }
        }

        command.current_dir(&ctx.workspace_dir);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        // Killing the child if the supervisor task is dropped is a hard
        // requirement of design.md §Engine so a panicking supervisor cannot
        // leak orphan worker subprocesses.
        command.kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| LaunchError::Spawn {
            binary: self.binary.clone(),
            source,
        })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or(LaunchError::MissingPipe { which: "stdin" })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(LaunchError::MissingPipe { which: "stdout" })?;

        // Hand the prelude envelope + prompt to the agent. The supervisor
        // closes stdin afterwards so the agent observes EOF on its prompt
        // input rather than blocking waiting for more bytes.
        if stdin.write_all(session_input.as_bytes()).await.is_err() {
            // Treat a broken stdin as a non-clean termination upstream; the
            // child will still produce an exit status so the loop below
            // reports the canonical outcome.
        }
        drop(stdin);

        let policy = ctx.policy;
        let last_event_at = Arc::new(Mutex::new(Instant::now()));
        let stall_flag = Arc::new(Mutex::new(false));

        // ---- Stall watchdog ------------------------------------------------
        // Polls `last_event_at` against the configured stall window
        // (req 5.3). On detection it sets `stall_flag` and kills the child;
        // the main loop then reports `WorkerOutcome::Stalled` instead of
        // whatever exit status the killed child reports.
        let stall_last = Arc::clone(&last_event_at);
        let stall_flag_for_task = Arc::clone(&stall_flag);
        let pid = child.id();
        let stall_handle = tokio::spawn(async move {
            // Track wall-clock origin so stall comparisons see a monotonic
            // duration regardless of when the child first emits an event.
            let started = Instant::now();
            loop {
                time::sleep(STALL_TICK_INTERVAL).await;
                let last = *stall_last.lock().await;
                let now = Instant::now();
                let last_elapsed = last.duration_since(started);
                let now_elapsed = now.duration_since(started);
                if let Some(_reason) = policy.detect_stall(last_elapsed, now_elapsed) {
                    let mut flag = stall_flag_for_task.lock().await;
                    if !*flag {
                        *flag = true;
                        tracing::warn!(
                            target: "engine.claude",
                            pid = ?pid,
                            stall_window_secs = policy.stall_window.as_secs(),
                            "claude worker stalled; terminating subprocess"
                        );
                    }
                    return;
                }
            }
        });

        // ---- stdout reader -------------------------------------------------
        // Drives the per-line parser and refreshes `last_event_at` on every
        // observed event so the watchdog sees forward progress (req 5.2,
        // 5.3).
        let mut reader = BufReader::new(stdout).lines();
        let stdout_last = Arc::clone(&last_event_at);
        let events_for_stream = events.clone();
        let stream_task = tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(event) = parse_line(&line) {
                    *stdout_last.lock().await = Instant::now();
                    // Task 3.4 / req 7.4: when the agent invokes a tool we
                    // record the observation under a stable target with only
                    // the tool name. Never log inputs or outputs through the
                    // engine's tracing path because they may contain
                    // daemon-owned credentials.
                    if let EngineLifecycleEvent::ToolCall { name } = &event {
                        tracing::info!(
                            target: "engine.claude.tools",
                            tool = name.as_str(),
                            "agent invoked tool (observed via stream-json)",
                        );
                    }
                    if events_for_stream
                        .send(SupervisedEvent::Lifecycle(event))
                        .await
                        .is_err()
                    {
                        // Receiver dropped; nothing to do. The caller
                        // observed enough events and walked away.
                        return;
                    }
                }
            }
        });

        // ---- Main supervisor loop ----------------------------------------
        // Wait for either the child to exit or the stall watchdog to fire.
        // A turn-budget-exhausted outcome is observed by the orchestrator at
        // continuation-prompt time; this single-launch supervisor reports
        // only what it can see directly: clean / non-clean exit, or stall.
        let outcome = tokio::select! {
            // Stall watchdog won the race: kill the child and report the
            // stall outcome (req 5.3). `start_kill` is best-effort; the
            // following `wait` drains the exit status.
            _ = stall_handle => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                WorkerOutcome::Stalled {
                    reason: StallReason::EventInactivity,
                }
            }
            // Subprocess exited on its own.
            wait_result = child.wait() => {
                match wait_result {
                    Ok(status) => {
                        if status.success() {
                            WorkerOutcome::CleanExit
                        } else {
                            // Map both real exit codes and signal-induced
                            // termination onto the policy's NonCleanExit
                            // variant. Following shell convention,
                            // signal-only terminations are reported as
                            // `128 + signal` so the operator can identify
                            // the cause from the structured log line.
                            let code = status.code().unwrap_or_else(|| {
                                #[cfg(unix)]
                                {
                                    use std::os::unix::process::ExitStatusExt;
                                    status.signal().map(|s| 128 + s).unwrap_or(-1)
                                }
                                #[cfg(not(unix))]
                                {
                                    -1
                                }
                            });
                            WorkerOutcome::NonCleanExit { code }
                        }
                    }
                    Err(_) => WorkerOutcome::NonCleanExit { code: -1 },
                }
            }
        };

        // Drain any final stream events so the orchestrator sees every line
        // the child managed to emit before the watchdog killed it.
        let _ = stream_task.await;

        // Re-check the stall flag in case the child happened to exit at the
        // same instant the watchdog fired; the stall outcome wins because
        // requirements.md §5.3 names event-inactivity as the canonical
        // termination reason in that race.
        let final_outcome = if *stall_flag.lock().await {
            WorkerOutcome::Stalled {
                reason: StallReason::EventInactivity,
            }
        } else {
            outcome
        };

        // Emit exactly one terminal Exited event (design.md §Engine).
        let _ = events.send(SupervisedEvent::Exited(final_outcome)).await;

        Ok(final_outcome)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the prelude envelope construction (Requirement 13.4).
    //!
    //! Subprocess lifecycle scenarios are exercised by the integration test
    //! at `crates/roki-daemon/tests/engine_claude.rs`, which drives a fake
    //! `claude` binary through clean-exit, non-clean-exit, and stall paths.

    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    use crate::engine::policy::EnginePolicy;
    use crate::permissions::{PermissionMode, PermissionSource, ResolvedPermission};
    use crate::workflow::{ElicitationsMode, SandboxMode};

    fn allowlist_permission() -> ResolvedPermission {
        ResolvedPermission {
            mode: PermissionMode::Allowlist {
                settings_path: PathBuf::from("/etc/roki/settings.json"),
            },
            sandbox: SandboxMode::WorkspaceWrite,
            elicitations: ElicitationsMode::Reject,
            mode_source: PermissionSource::Operator,
        }
    }

    fn ctx_with_additional(additional: Option<serde_json::Value>) -> WorkerContext {
        WorkerContext {
            repo: RepoId::new("repo-x"),
            issue: IssueId::new("ENG-7"),
            correlation_id: CorrelationId::from_uuid(Uuid::nil()),
            workspace_dir: PathBuf::from("/tmp/roki-ws"),
            prompt: "hello agent".to_owned(),
            tool_catalog: Vec::new(),
            permission: allowlist_permission(),
            policy: EnginePolicy::default(),
            additional_context: additional,
        }
    }

    #[test]
    fn additional_context_appears_verbatim_in_prelude() {
        // Requirement 13.4: when `additional_context` is `Some(value)`, the
        // value is forwarded verbatim through the prelude envelope.
        let value = serde_json::json!({
            "foo": "bar",
            "nested": {"answer": 42, "list": [1, 2, 3]},
        });
        let ctx = ctx_with_additional(Some(value.clone()));

        let session = build_session_input(&ctx);

        // The bytes contain both sentinels and the verbatim JSON value under
        // the documented stable key.
        assert!(
            session.contains(PRELUDE_OPEN),
            "prelude envelope must include opening sentinel, got:\n{session}",
        );
        assert!(
            session.contains(PRELUDE_CLOSE),
            "prelude envelope must include closing sentinel, got:\n{session}",
        );
        assert!(
            session.contains(r#""foo":"bar""#),
            "additional_context must appear verbatim, got:\n{session}",
        );
        assert!(
            session.contains(r#""answer":42"#),
            "nested numeric values must round-trip verbatim, got:\n{session}",
        );

        // Locate the JSON body between the sentinels and assert the value
        // round-trips through serde under the documented stable key.
        let body = extract_prelude_body(&session);
        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("prelude body must be valid JSON");
        assert_eq!(
            parsed
                .get(PRELUDE_ADDITIONAL_CONTEXT_KEY)
                .expect("prelude must carry the documented stable key"),
            &value,
            "additional_context must round-trip verbatim under the stable key",
        );
    }

    #[test]
    fn additional_context_absent_omits_the_key() {
        // The `additional_context` key is skipped when `None` so the envelope
        // stays minimal in the common case (matches the spec's "additive
        // optional field" wording in Requirement 13.4).
        let ctx = ctx_with_additional(None);
        let session = build_session_input(&ctx);
        let body = extract_prelude_body(&session);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert!(
            parsed.get(PRELUDE_ADDITIONAL_CONTEXT_KEY).is_none(),
            "additional_context must be absent when ctx.additional_context is None, got: {parsed}",
        );
    }

    #[test]
    fn prompt_follows_the_closing_sentinel() {
        // The prompt is appended after the closing sentinel so downstream
        // consumers can split on the sentinel pair without a JSON parser.
        let ctx = ctx_with_additional(None);
        let session = build_session_input(&ctx);

        let close_idx = session
            .find(PRELUDE_CLOSE)
            .expect("closing sentinel must be present");
        let after_close = &session[close_idx + PRELUDE_CLOSE.len()..];
        assert!(
            after_close.contains(&ctx.prompt),
            "prompt must follow the closing sentinel, after_close = {after_close:?}",
        );
    }

    #[test]
    fn tool_catalog_round_trips_through_the_prelude() {
        // Requirement 7.1 / 7.5: the tool catalog reaches every worker
        // subprocess at launch. The prelude is the documented forwarding
        // channel.
        let mut ctx = ctx_with_additional(None);
        ctx.tool_catalog = vec![ToolDescriptor {
            name: "echo",
            description: "noop",
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: serde_json::json!({"type":"object"}),
        }];

        let body = extract_prelude_body(&build_session_input(&ctx));
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let tools = parsed
            .get(PRELUDE_TOOLS_KEY)
            .and_then(|v| v.as_array())
            .expect("tools array must be present under the stable key");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo");
    }

    /// Locate the JSON body inside the prelude sentinels. Helper for tests
    /// that need to assert against the parsed envelope.
    fn extract_prelude_body(session: &str) -> String {
        let open_idx = session
            .find(PRELUDE_OPEN)
            .expect("opening sentinel must be present");
        let after_open = &session[open_idx + PRELUDE_OPEN.len()..];
        let close_rel = after_open
            .find(PRELUDE_CLOSE)
            .expect("closing sentinel must be present");
        after_open[..close_rel].trim().to_owned()
    }
}
