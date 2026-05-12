//! Typed error surface for the roki walking-skeleton daemon.
//!
//! All module-level error enums are aggregated here so that later tasks can
//! fill the per-module stub files (`config/roki.rs`, `config/workflow.rs`,
//! `linear/client.rs`, `linear/webhook.rs`, `admission.rs`, `capture.rs`,
//! `runner.rs`) in parallel without colliding on the error definitions.
//!
//! Each variant carries the offending cause (file path, key path, bind
//! address, GraphQL endpoint, ticket id, or correlation id) so the
//! `tracing::error!` line and exit-code path can identify it.

use std::path::PathBuf;

use thiserror::Error;

/// Errors raised while loading `roki.toml`.
///
/// Covers requirement 1.2 (missing config path) and 2.3 (schema validation
/// with key-path-bearing message).
#[derive(Debug, Error)]
pub enum RokiConfigError {
    #[error("roki.toml not found: {path}")]
    MissingFile { path: PathBuf },

    #[error("roki.toml unreadable at {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("roki.toml parse error at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("roki.toml at {path} missing required key '{key}'")]
    MissingField { path: PathBuf, key: String },

    #[error("roki.toml at {path} key '{key}' has wrong type, expected {expected}")]
    TypeMismatch {
        path: PathBuf,
        key: String,
        expected: &'static str,
    },
}

/// Errors raised while loading `WORKFLOW.yaml` or `workflow/*.md` frontmatter.
#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("WORKFLOW.yaml not found: {path}")]
    MissingFile { path: PathBuf },

    #[error("WORKFLOW.yaml unreadable at {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("WORKFLOW.yaml parse error at {path}: {detail}")]
    YamlParse { path: PathBuf, detail: String },

    #[error("WORKFLOW.yaml validation error at {path}: {detail}")]
    YamlValidation { path: PathBuf, detail: String },

    #[error("WORKFLOW.yaml at {path} missing required field '{field}'")]
    MissingField { path: PathBuf, field: String },

    #[error("invalid workflow.toml at {path}: unsupported when.* key '{key}'")]
    UnsupportedWhen { path: PathBuf, key: String },

    #[error("invalid workflow.toml at {path}: unsupported run.* form '{key}'")]
    UnsupportedRunForm { path: PathBuf, key: String },

    #[error(
        "invalid workflow.toml at {path}: run phase resolved to session shape \
         (slice-2 unsupported; lift via path-form .md frontmatter `session: \"command\"`)"
    )]
    SessionRunUnsupported { path: PathBuf },

    #[error("[[cleanup]][{index}] declares pre/post but no run; in {path}")]
    CleanupMissingRun {
        path: std::path::PathBuf,
        index: usize,
    },
    #[error("[[cleanup]][{index}] is shorthand (no phases) but declares when.*; in {path}")]
    CleanupShorthandWithWhen {
        path: std::path::PathBuf,
        index: usize,
    },

    #[error(
        "invalid workflow .md frontmatter at {path}: \
         field 'session' has unsupported value '{value}' (allowed: \"session\", \"command\")"
    )]
    InvalidSessionField { path: PathBuf, value: String },

    #[error(
        "invalid workflow .md frontmatter at {path}: \
         field 'stall_seconds' must be an integer >= 1, got '{value}'"
    )]
    InvalidStallSeconds { path: PathBuf, value: String },

    #[error("workflow .md frontmatter parse error at {path}: {reason}")]
    WorkflowMdFrontmatter { path: PathBuf, reason: String },

    #[error("[[on_failure]][{index}] missing run; in {path}")]
    OnFailureMissingRun {
        path: std::path::PathBuf,
        index: usize,
    },
    #[error("[[on_failure]][{index}] missing when.kind; in {path}")]
    OnFailureMissingKind {
        path: std::path::PathBuf,
        index: usize,
    },
    #[error(
        "[[on_failure]][{index}] sets multiple of when.kind / when.kind.in / when.kind.not; in {path}"
    )]
    OnFailureKindMatcherConflict {
        path: std::path::PathBuf,
        index: usize,
    },
    #[error("[[on_failure]][{index}] when.kind = {value:?} not in legal set; in {path}")]
    OnFailureUnknownKind {
        path: std::path::PathBuf,
        index: usize,
        value: String,
    },
    #[error("[[on_failure]][{index}] when.kind.in is empty; in {path}")]
    OnFailureEmptyKindIn {
        path: std::path::PathBuf,
        index: usize,
    },
}

/// Errors raised by the Linear GraphQL client during `viewer { id }` resolve.
///
/// Covers Req 4.2 — the daemon must resolve the viewer id at startup and
/// non-200, malformed body, or missing `viewer.id` is fatal with the
/// endpoint identified in the log line.
#[derive(Debug, Error)]
pub enum LinearClientError {
    #[error("linear graphql request failed for endpoint {endpoint}: {source}")]
    Http {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("linear viewer resolve failed at {endpoint}: {reason}")]
    ViewerResolveFailed { endpoint: String, reason: String },
}

/// Errors raised by the paginated Linear GraphQL `issues(...)` enumeration
/// primitive used by cold start (slice 6) and, in a future slice, by
/// polling.
///
/// Distinct from `LinearClientError` (which models the slice-1 one-shot
/// `viewer { id }` resolve at startup) so the enumeration's failure modes
/// — pagination, 429 backoff exhaustion, GraphQL-level `errors` arrays —
/// stay typed independently. Each variant carries the offending endpoint
/// (or, for `BackoffExhausted`, the retry count) so the `tracing::error!`
/// line can identify the cause from the error alone.
#[derive(Debug, Error)]
pub enum LinearEnumerateError {
    #[error("linear graphql request failed for endpoint {endpoint}: {source}")]
    Http {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("linear graphql non-success status {status} at {endpoint}")]
    NonSuccess { endpoint: String, status: u16 },

    #[error("linear graphql malformed response from {endpoint}: {reason}")]
    Malformed { endpoint: String, reason: String },

    #[error("linear graphql errors[] from {endpoint}: {message}")]
    GraphqlError { endpoint: String, message: String },

    #[error("linear graphql 429 backoff exhausted after {retries} retries")]
    BackoffExhausted { retries: u32 },
}

/// Errors raised by the webhook listener.
///
/// `BindFailed` covers Req 3.1 (listener bind on the configured port).
/// `InvalidPayload` covers Req 3.4 — bad payloads return HTTP 400 with a
/// warn-log carrying a generated `error_id` for log correlation; the
/// listener stays open.
#[derive(Debug, Error)]
pub enum WebhookError {
    #[error("webhook listener failed to bind {addr}: {source}")]
    BindFailed {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid webhook payload (error_id={error_id}): {reason}")]
    InvalidPayload { error_id: String, reason: String },
}

/// Errors / outcomes raised by the admission filter.
///
/// `AssigneeMismatch` (Req 4.1) is an info-log outcome rather than a fatal
/// daemon error — it is still typed here so the runtime can pattern-match
/// on it without stringly comparisons. `NoRepos` (Req 4.4) is fatal at
/// config load time.
#[derive(Debug, Error)]
pub enum AdmissionError {
    #[error("ticket {ticket_id} assignee mismatch: expected {expected}, got {got:?}")]
    AssigneeMismatch {
        ticket_id: String,
        expected: String,
        got: Option<String>,
    },

    #[error("admission has no [[admission.repos]] entries")]
    NoRepos,
}

/// Errors raised by the per-cycle capture layout.
///
/// All three variants cover Req 7.3 — any filesystem failure when creating
/// the per-cycle directory or writing stdout/stderr is fatal with the
/// offending path identified.
#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("capture failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("capture failed to open file {path}: {source}")]
    OpenFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("capture failed to write to {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Engine-side infrastructure errors. Slice 8 keeps a slim subset (slice 1
/// session/phase variants are gone) covering subprocess spawn, ghq lookup,
/// worktree ops, and capture-fs failures. Surface to the cycle driver as
/// `FailureKind::FsPoison` / `ProcessCrash` / `TemplateError` per the
/// translation in `engine::real_state_runner`.
#[derive(Debug, Error)]
pub enum PhaseInfraError {
    #[error("phase failed to spawn '{cmd}': {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("phase failed to wait on '{cmd}': {source}")]
    Wait {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("phase failed to read workflow body at {path}: {source}")]
    WorkflowBodyUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("phase '{cmd}' has no stdin handle but a rendered stdin body was prepared")]
    StdinUnavailable { cmd: String },

    #[error("phase failed to write stdin for '{cmd}': {source}")]
    StdinWrite {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("ghq base path not found for '{ghq}'")]
    RepoNotFound { ghq: String },

    /// Worktree create / list / remove failed before subprocess launch.
    /// Cycle driver converts this to FailureKind::FsPoison.
    #[error("worktree operation failed: {error_text}")]
    WorktreeError {
        error_text: String,
        exit_code: Option<i32>,
    },

    #[error(transparent)]
    Capture(#[from] CaptureError),
}

/// Top-level aggregate error for the skeleton daemon.
///
/// Each module's typed error converts via `#[from]`. The runtime maps each
/// variant to the appropriate `tracing` level and exit code per the design
/// "Error Categories and Responses" table.
#[derive(Debug, Error)]
pub enum SkeletonError {
    #[error(transparent)]
    Config(#[from] RokiConfigError),

    #[error(transparent)]
    Workflow(#[from] WorkflowError),

    #[error(transparent)]
    LinearClient(#[from] LinearClientError),

    #[error(transparent)]
    Webhook(#[from] WebhookError),

    #[error(transparent)]
    Admission(#[from] AdmissionError),

    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error(transparent)]
    PhaseInfra(#[from] PhaseInfraError),

    #[error("shutdown window exceeded; aborted in-flight ticket tasks")]
    ShutdownWindowExceeded,

    #[error("missing required CLI dependency: {}", binaries.join(", "))]
    MissingDependency { binaries: Vec<String> },
}

#[cfg(test)]
mod tests {
    //! Display tests prove each error message identifies the offending
    //! cause (path, key, addr, endpoint, ticket_id, error_id, cmd).
    //! These satisfy the design contract that `tracing::error!` lines can
    //! identify the cause from the error alone.

    use super::*;
    use std::io;
    use std::path::PathBuf;

    fn io_err() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "x")
    }

    #[test]
    fn roki_config_display_carries_path_and_key() {
        let e = RokiConfigError::MissingFile {
            path: PathBuf::from("/tmp/roki.toml"),
        };
        assert!(format!("{e}").contains("/tmp/roki.toml"));

        let e = RokiConfigError::Unreadable {
            path: PathBuf::from("/tmp/roki.toml"),
            source: io_err(),
        };
        assert!(format!("{e}").contains("/tmp/roki.toml"));

        let e = RokiConfigError::MissingField {
            path: PathBuf::from("/tmp/roki.toml"),
            key: "linear.token".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("/tmp/roki.toml"));
        assert!(s.contains("linear.token"));

        let e = RokiConfigError::TypeMismatch {
            path: PathBuf::from("/tmp/roki.toml"),
            key: "webhook.port".into(),
            expected: "u16",
        };
        let s = format!("{e}");
        assert!(s.contains("webhook.port"));
        assert!(s.contains("u16"));
    }

    #[test]
    fn workflow_display_identifies_yaml_failures() {
        let e = WorkflowError::YamlParse {
            path: PathBuf::from("/tmp/WORKFLOW.yaml"),
            detail: "expected mapping".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("/tmp/WORKFLOW.yaml"));
        assert!(s.contains("expected mapping"));

        let e = WorkflowError::YamlValidation {
            path: PathBuf::from("/tmp/WORKFLOW.yaml"),
            detail: "rule[0]: orphan target".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("orphan target"));

        let e = WorkflowError::MissingField {
            path: PathBuf::from("/tmp/WORKFLOW.yaml"),
            field: "admission".into(),
        };
        assert!(format!("{e}").contains("admission"));
    }

    #[test]
    fn linear_client_display_carries_endpoint() {
        let e = LinearClientError::ViewerResolveFailed {
            endpoint: "https://api.linear.app/graphql".into(),
            reason: "missing viewer.id".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("https://api.linear.app/graphql"));
        assert!(s.contains("missing viewer.id"));
    }

    #[test]
    fn webhook_display_carries_addr_and_error_id() {
        let e = WebhookError::BindFailed {
            addr: "127.0.0.1:8080".into(),
            source: io_err(),
        };
        assert!(format!("{e}").contains("127.0.0.1:8080"));

        let e = WebhookError::InvalidPayload {
            error_id: "abc-123".into(),
            reason: "missing state.name".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("abc-123"));
        assert!(s.contains("missing state.name"));
    }

    #[test]
    fn admission_display_carries_ticket_id() {
        let e = AdmissionError::AssigneeMismatch {
            ticket_id: "ENG-42".into(),
            expected: "u1".into(),
            got: Some("u2".into()),
        };
        let s = format!("{e}");
        assert!(s.contains("ENG-42"));
        assert!(s.contains("u1"));
        assert!(s.contains("u2"));

        let e = AdmissionError::NoRepos;
        assert!(format!("{e}").contains("admission.repos"));
    }

    #[test]
    fn capture_display_carries_path() {
        let e = CaptureError::CreateDir {
            path: PathBuf::from("/var/roki/cycle-1"),
            source: io_err(),
        };
        assert!(format!("{e}").contains("/var/roki/cycle-1"));

        let e = CaptureError::OpenFile {
            path: PathBuf::from("/var/roki/cycle-1/stdout.log"),
            source: io_err(),
        };
        assert!(format!("{e}").contains("stdout.log"));

        let e = CaptureError::Write {
            path: PathBuf::from("/var/roki/cycle-1/stderr.log"),
            source: io_err(),
        };
        assert!(format!("{e}").contains("stderr.log"));
    }

    #[test]
    fn phase_infra_display_carries_paths_and_cmds() {
        let e = PhaseInfraError::Spawn {
            cmd: "claude --foo".into(),
            source: io_err(),
        };
        assert!(format!("{e}").contains("claude --foo"));

        let e = PhaseInfraError::Wait {
            cmd: "claude --foo".into(),
            source: io_err(),
        };
        assert!(format!("{e}").contains("claude --foo"));

        let e = PhaseInfraError::RepoNotFound {
            ghq: "github.com/acme/widget".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("github.com/acme/widget"), "msg: {s}");

        let e = PhaseInfraError::Capture(CaptureError::CreateDir {
            path: PathBuf::from("/tmp/foo"),
            source: io_err(),
        });
        assert!(format!("{e}").contains("/tmp/foo"));

        let e = PhaseInfraError::WorktreeError {
            error_text: "wt remove failed".into(),
            exit_code: Some(1),
        };
        assert!(format!("{e}").contains("wt remove failed"));
    }

    #[test]
    fn skeleton_error_aggregates_phase_infra() {
        let inner = PhaseInfraError::Spawn {
            cmd: "x".into(),
            source: io_err(),
        };
        let outer: SkeletonError = inner.into();
        assert!(format!("{outer}").contains("x"));
    }

    #[test]
    fn skeleton_error_aggregates_via_from() {
        let inner = RokiConfigError::MissingFile {
            path: PathBuf::from("/tmp/roki.toml"),
        };
        let outer: SkeletonError = inner.into();
        // transparent forwarding: outer Display matches inner Display.
        assert!(format!("{outer}").contains("/tmp/roki.toml"));

        let inner = WorkflowError::UnsupportedWhen {
            path: PathBuf::from("/tmp/WORKFLOW.toml"),
            key: "when.assignee".into(),
        };
        let outer: SkeletonError = inner.into();
        assert!(format!("{outer}").contains("when.assignee"));

        let inner = LinearClientError::ViewerResolveFailed {
            endpoint: "https://api.linear.app/graphql".into(),
            reason: "non-200".into(),
        };
        let outer: SkeletonError = inner.into();
        assert!(format!("{outer}").contains("https://api.linear.app/graphql"));

        let inner = WebhookError::InvalidPayload {
            error_id: "id-1".into(),
            reason: "bad".into(),
        };
        let outer: SkeletonError = inner.into();
        assert!(format!("{outer}").contains("id-1"));

        let inner = AdmissionError::AssigneeMismatch {
            ticket_id: "ENG-1".into(),
            expected: "u1".into(),
            got: None,
        };
        let outer: SkeletonError = inner.into();
        assert!(format!("{outer}").contains("ENG-1"));

        let inner = CaptureError::CreateDir {
            path: PathBuf::from("/tmp/c"),
            source: io_err(),
        };
        let outer: SkeletonError = inner.into();
        assert!(format!("{outer}").contains("/tmp/c"));
    }
}
