//! Production wiring of [`OrchestratorSessionAdapter`] to the orchestrator
//! core's [`OrchestratorEngine`] seam.
//!
//! Why this lives in a separate module: the trait expects a self-contained
//! `launch(&issue, mode, system_prompt)` call, but the underlying adapter
//! also needs the per-issue session tempdir and the resolved orchestrator
//! `allowed_tools` list. [`OrchestratorEngineImpl`] composes those
//! collaborators so the trait stays narrow while the adapter's concrete
//! API is unchanged.
//!
//! The wrapper is pure routing: it never inspects the action stream or
//! mutates the adapter's state. The trait-side
//! [`OrchestratorSessionLike`] handle is implemented by
//! [`SessionLikeHandle`], which wraps the adapter's
//! [`OrchestratorSessionHandle`] and translates [`ActionEvent`] into the
//! orchestrator core's [`OrchestratorActionEvent`].
//!
//! Spec refs: requirements.md Req 4.1, 5.1, 7.1, 7.3, 13.1, 13.2; design.md
//! "Daemon bootstrap" step 8.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::engine::orchestrator_session::adapter::{
    ActionEvent, OrchestratorLaunchContext, OrchestratorSessionAdapter, OrchestratorSessionHandle,
};
use crate::engine::orchestrator_session::events::DaemonEvent;
use crate::logging::DebugSinkFactory;
use crate::orchestrator::core::{
    DeliveryError, EngineError, OrchestratorActionEvent, OrchestratorEngine,
    OrchestratorSessionLike,
};
use crate::orchestrator::state::{IssueId, Mode};
use crate::session::SessionManager;

/// Production [`OrchestratorEngine`] wired around the long-lived
/// [`OrchestratorSessionAdapter`]. Holds the additional collaborators the
/// adapter needs to assemble an [`OrchestratorLaunchContext`].
pub struct OrchestratorEngineImpl {
    adapter: Arc<OrchestratorSessionAdapter>,
    session_manager: Arc<SessionManager>,
    allowed_tools: Vec<String>,
    /// When `Some`, every launch materializes a per-issue debug sink via
    /// [`DebugSinkFactory::for_issue`] so the orchestrator's stdout / stderr
    /// lines are appended to `<debug_dir>/<issue>.log` per Req 11.6 / 11.7.
    /// `None` disables the per-issue capture (default unless the operator
    /// passes `--debug` or sets `[debug].dir`).
    debug_sink_factory: Option<Arc<DebugSinkFactory>>,
}

impl OrchestratorEngineImpl {
    pub fn new(
        adapter: Arc<OrchestratorSessionAdapter>,
        session_manager: Arc<SessionManager>,
        allowed_tools: Vec<String>,
        debug_sink_factory: Option<Arc<DebugSinkFactory>>,
    ) -> Self {
        Self {
            adapter,
            session_manager,
            allowed_tools,
            debug_sink_factory,
        }
    }
}

#[async_trait]
impl OrchestratorEngine for OrchestratorEngineImpl {
    async fn launch(
        &self,
        issue: &IssueId,
        mode: Mode,
        system_prompt: String,
    ) -> Result<Box<dyn OrchestratorSessionLike>, EngineError> {
        // The orchestrator core has already ensured the session tempdir via
        // its `SessionDirOps` seam before calling `launch` (see
        // `orchestrator::core::handle_admit`). We re-look it up here rather
        // than threading the path through the seam signature so the trait
        // stays narrow.
        let session_tempdir = self
            .session_manager
            .ensure(issue)
            .map_err(|err| EngineError::Internal(err.to_string()))?;

        let ctx = OrchestratorLaunchContext {
            issue: issue.clone(),
            mode,
            session_tempdir,
            system_prompt,
            allowed_tools: self.allowed_tools.clone(),
            // Per-issue debug sink is materialized when the runtime composed
            // a `DebugSinkFactory` from `--debug` / `[debug].dir`
            // (Req 11.6, 11.7). When the factory is absent, the launch keeps
            // its default no-capture behavior.
            debug_sink: self
                .debug_sink_factory
                .as_ref()
                .map(|factory| factory.for_issue(issue.0.as_str())),
        };
        let handle = self
            .adapter
            .launch(ctx)
            .await
            .map_err(|err| EngineError::LaunchFailed(err.to_string()))?;
        Ok(Box::new(SessionLikeHandle { inner: handle }))
    }
}

/// Bridge from the adapter's [`OrchestratorSessionHandle`] to the
/// orchestrator core's [`OrchestratorSessionLike`] trait. Translates
/// [`ActionEvent`] -> [`OrchestratorActionEvent`].
struct SessionLikeHandle {
    inner: OrchestratorSessionHandle,
}

#[async_trait]
impl OrchestratorSessionLike for SessionLikeHandle {
    async fn deliver(&self, event: DaemonEvent) -> Result<(), DeliveryError> {
        self.inner
            .stdin_tx
            .send(event)
            .await
            .map_err(|_| DeliveryError::Closed)
    }

    async fn next_action(&mut self) -> Option<OrchestratorActionEvent> {
        loop {
            match self.inner.action_rx.recv().await? {
                ActionEvent::Action(action) => {
                    return Some(OrchestratorActionEvent::Action(action));
                }
                ActionEvent::Drift { reprompt: _ } => {
                    // First-time drift is an internal reprompt: the parser
                    // will re-emit on the next turn. Loop and wait for the
                    // next event without surfacing to the orchestrator core.
                    continue;
                }
                ActionEvent::TerminalDrift { raw_stdout: _ } => {
                    return Some(OrchestratorActionEvent::TerminalDrift);
                }
                ActionEvent::ProcessExit { status, raw_stdout: _ } => {
                    return Some(OrchestratorActionEvent::ProcessExit {
                        success: status.success(),
                    });
                }
            }
        }
    }

    async fn shutdown(self: Box<Self>, grace: Option<Duration>) {
        let _status = self.inner.shutdown(grace).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::engine::claude::ClaudeBinary;
    use crate::permissions::PermissionResolver;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    fn fake_claude_path() -> &'static std::path::Path {
        static PATH: OnceLock<PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            let status = std::process::Command::new(env!("CARGO"))
                .args(["build", "--quiet", "--example", "fake_claude", "-p", "roki-daemon"])
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
        })
        .as_path()
    }

    fn write_mode(dir: &std::path::Path, mode: &str) {
        std::fs::write(dir.join(".fake_claude_mode"), mode).unwrap();
    }

    /// When the engine impl is constructed with a `DebugSinkFactory`,
    /// every launch must materialize a per-issue sink so the orchestrator
    /// session's stdout / stderr lines flow into `<dir>/<issue>.log` per
    /// Req 11.6. Drives the production wiring through the fake_claude
    /// harness so this test exercises the full launch -> stdout-drainer
    /// pipeline rather than only the field assignment.
    #[tokio::test]
    async fn launch_with_debug_sink_factory_writes_per_issue_log_file() {
        let session_root = tempfile::tempdir().expect("session_root tempdir");
        let session_manager = Arc::new(crate::session::SessionManager::with_root(
            session_root.path().to_path_buf(),
        ));

        // The session tempdir must exist BEFORE the launch so fake_claude can
        // resolve `.fake_claude_mode` from CWD; the engine impl ensures it
        // for us via session_manager.ensure(...).
        let issue = IssueId::from("ENG-DEBUG-1");
        let session_dir = session_manager.ensure(&issue).expect("ensure session dir");
        write_mode(&session_dir, "single_action");

        let debug_dir = tempfile::tempdir().expect("debug_dir tempdir");
        let factory = Arc::new(crate::logging::DebugSinkFactory::new(
            debug_dir.path().to_path_buf(),
        ));

        let binary = ClaudeBinary::discover(Some(fake_claude_path()))
            .expect("fake_claude discoverable");
        let permissions = PermissionResolver::resolve_for_orchestrator(&[
            "Read".to_owned(),
            "mcp__linear*".to_owned(),
        ]);
        let adapter = Arc::new(OrchestratorSessionAdapter::new(binary, permissions));

        let engine = OrchestratorEngineImpl::new(
            adapter,
            session_manager,
            vec!["Read".to_owned()],
            Some(factory),
        );

        let mut session = engine
            .launch(&issue, Mode::SpecDriven, "TEST-PROMPT".to_owned())
            .await
            .expect("engine launch");

        // Drain at least one action so the stdout drainer task has had a
        // chance to flush a line into the per-issue debug sink.
        let _ = tokio::time::timeout(Duration::from_secs(5), session.next_action()).await;

        // Shutting down forces the IO tasks to drain and the per-issue sink
        // to flush its writes before the file handle goes out of scope.
        session.shutdown(Some(Duration::from_secs(3))).await;

        let log_path = debug_dir.path().join("ENG-DEBUG-1.log");
        assert!(
            log_path.exists(),
            "per-issue debug log must exist at {log_path:?}"
        );
        let body = std::fs::read_to_string(&log_path).expect("read per-issue log");
        assert!(
            !body.is_empty(),
            "per-issue debug log must contain at least one captured line; got empty"
        );
        // Documented format: `<RFC3339-nano> [STDOUT|STDERR] orchestrator <line>`.
        let pattern = regex::Regex::new(
            r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z \[(STDOUT|STDERR)\] orchestrator ",
        )
        .expect("compile log line regex");
        let matched = body.lines().any(|line| pattern.is_match(line));
        assert!(
            matched,
            "per-issue debug log must contain at least one line in the documented format; body = {body}"
        );
    }

    /// Symmetric negative control: when the engine impl is constructed
    /// with `None`, no per-issue debug log file is written (existing
    /// behavior pre-Task 10.6).
    #[tokio::test]
    async fn launch_without_factory_writes_no_per_issue_log_file() {
        let session_root = tempfile::tempdir().expect("session_root tempdir");
        let session_manager = Arc::new(crate::session::SessionManager::with_root(
            session_root.path().to_path_buf(),
        ));

        let issue = IssueId::from("ENG-DEBUG-2");
        let session_dir = session_manager.ensure(&issue).expect("ensure session dir");
        write_mode(&session_dir, "single_action");

        let debug_dir = tempfile::tempdir().expect("debug_dir tempdir");

        let binary = ClaudeBinary::discover(Some(fake_claude_path()))
            .expect("fake_claude discoverable");
        let permissions = PermissionResolver::resolve_for_orchestrator(&[
            "Read".to_owned(),
            "mcp__linear*".to_owned(),
        ]);
        let adapter = Arc::new(OrchestratorSessionAdapter::new(binary, permissions));

        let engine = OrchestratorEngineImpl::new(
            adapter,
            session_manager,
            vec!["Read".to_owned()],
            None,
        );

        let mut session = engine
            .launch(&issue, Mode::SpecDriven, "TEST-PROMPT".to_owned())
            .await
            .expect("engine launch");
        let _ = tokio::time::timeout(Duration::from_secs(5), session.next_action()).await;
        session.shutdown(Some(Duration::from_secs(3))).await;

        let log_path = debug_dir.path().join("ENG-DEBUG-2.log");
        assert!(
            !log_path.exists(),
            "per-issue debug log must NOT exist when factory is None; got {log_path:?}"
        );
    }
}
