//! Layered configuration loader for `roki.toml`.
//!
//! Schema canonical: [`docs/reference/config.md`](../../../../docs/reference/config.md).
//! Boot composition: [`design.md` "Daemon bootstrap" steps 1-7].
//!
//! Responsibilities of this module:
//! - Parse `roki.toml` into a typed `Config`.
//! - Refuse legacy `[judge].model`, `extension.linear_updater.*`,
//!   `extension.gates.*`, and `extension.distill.*` keys per Req 2.12.
//! - Refuse duplicate `[[repos]].ghq` per Req 2.2.
//! - Refuse empty `[linear].assignee` per Req 2.9 and empty resolved
//!   `[linear].admit_states` per Req 2.10.
//! - Resolve secrets (`SecretSource::Env` / `SecretSource::File`) through an
//!   injectable `EnvReader` so tests need no `unsafe` `set_var`.
//! - Validate `[workflow].path` is readable per Req 2.4.
//!
//! Comments only call out the non-obvious "why" the rule exists, never restate
//! the rule itself.

pub mod repos;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use toml::Value;

pub use repos::{DuplicateRepo, RepoEntry};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every configuration failure carries an actionable message naming the
/// offending key path so the daemon log entry on refusal points the operator
/// at exactly one field.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file at {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed roki.toml: {0}")]
    Parse(String),

    #[error("missing required configuration block `[{block}]`")]
    MissingBlock { block: &'static str },

    #[error(
        "legacy configuration key `{key}` is no longer supported; both judge \
         classification and Linear writes are now performed by the orchestrator \
         session — see docs/fr/19-orchestrator-session.md"
    )]
    LegacyKey { key: String },

    #[error("`[linear].assignee` is empty; set it to `me` or to a Linear user selector")]
    EmptyAssignee,

    #[error(
        "`[linear].admit_states` resolved to an empty set; either omit the key (default \
         [\"Todo\"]) or list at least one Linear workflow state name"
    )]
    EmptyAdmitStates,

    #[error(
        "duplicate `[[repos]].ghq = \"{ghq}\"` declared at entries #{first} and #{second}; \
         remove one"
    )]
    DuplicateRepo {
        ghq: String,
        first: usize,
        second: usize,
    },

    #[error("environment variable `{var}` referenced by config is not set")]
    EnvVarMissing { var: String },

    #[error("failed to read secret file at {path}: {source}")]
    SecretFileUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("`[workflow].path = {path}` is missing or unreadable: {source}")]
    WorkflowUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Secret resolution
// ---------------------------------------------------------------------------

/// Secrets are sourced from one of two indirection forms (`{ env = "VAR" }` or
/// `{ file = "/path" }`) per `docs/reference/config.md`. The literal-inline
/// form is intentionally not modeled — operators are expected to keep secrets
/// out of the TOML source.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SecretSource {
    #[serde(rename = "env")]
    Env(String),
    #[serde(rename = "file")]
    File(PathBuf),
}

/// Wrapper around a resolved secret. `Display` and `Debug` both redact, so the
/// only way to retrieve the cleartext value is through `expose_secret`. This
/// keeps the value out of `tracing` events even when a future log line
/// accidentally formats the wrapper directly.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(***REDACTED***)")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***REDACTED***")
    }
}

/// Indirection so tests can stub environment lookups without invoking
/// `std::env::set_var`, which is `unsafe` in edition 2024 and forbidden under
/// the workspace `unsafe_code = "forbid"` lint.
pub trait EnvReader {
    fn get(&self, var: &str) -> Option<String>;
}

/// Production reader: forwards to `std::env::var`.
pub struct ProcessEnv;

impl EnvReader for ProcessEnv {
    fn get(&self, var: &str) -> Option<String> {
        std::env::var(var).ok()
    }
}

/// In-memory env stand-in for tests.
#[derive(Default)]
pub struct StaticEnv {
    inner: BTreeMap<String, String>,
}

impl StaticEnv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner.insert(key.into(), value.into());
        self
    }
}

impl EnvReader for StaticEnv {
    fn get(&self, var: &str) -> Option<String> {
        self.inner.get(var).cloned()
    }
}

impl SecretSource {
    pub fn resolve(&self, env: &dyn EnvReader) -> Result<SecretValue, ConfigError> {
        match self {
            Self::Env(var) => env
                .get(var)
                .map(SecretValue::new)
                .ok_or_else(|| ConfigError::EnvVarMissing { var: var.clone() }),
            Self::File(path) => fs::read_to_string(path)
                .map(|contents| SecretValue::new(contents.trim_end_matches(['\n', '\r']).to_owned()))
                .map_err(|source| ConfigError::SecretFileUnreadable {
                    path: path.clone(),
                    source,
                }),
        }
    }
}

// ---------------------------------------------------------------------------
// Typed config blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub linear: LinearConfig,
    pub workflow: WorkflowConfig,
    pub server: ServerConfig,
    pub debug: DebugConfig,
    pub permissions: PermissionsConfig,
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearConfig {
    pub api_token: SecretSource,
    pub webhook_secret: SecretSource,
    pub assignee: AssigneeSpec,
    pub admit_states: BTreeSet<String>,
}

/// Carries the raw operator string. `"me"` is a placeholder resolved later by
/// runtime against the Linear viewer; the loader does not perform any lookup
/// (per task 1.3 spec — runtime resolution is task 3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssigneeSpec {
    Me,
    Selector(String),
}

impl AssigneeSpec {
    pub fn raw(&self) -> &str {
        match self {
            Self::Me => "me",
            Self::Selector(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConfig {
    pub bind: Option<String>,
    pub port: Option<u16>,
}

/// Daemon-internal `--debug` capture target. Documented in `tasks.md` task 1.3
/// even though it is not part of `ref:config` — this block governs per-issue
/// log capture rather than runtime behavior surfaced to the operator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugConfig {
    pub dir: Option<PathBuf>,
    pub level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionsConfig {
    pub strategy: PermissionStrategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStrategy {
    /// Phase subprocesses launched with `--settings <allowlist>`.
    SettingsAllowlist,
    /// Phase subprocesses launched with `--dangerously-skip-permissions`.
    DangerouslySkipPermissions,
}

// ---------------------------------------------------------------------------
// Raw deserialization shape (TOML → typed structs)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawConfig {
    linear: Option<RawLinear>,
    workflow: Option<RawWorkflow>,
    #[serde(default)]
    server: Option<RawServer>,
    #[serde(default)]
    debug: Option<RawDebug>,
    permissions: Option<RawPermissions>,
    #[serde(default)]
    repos: Vec<RepoEntry>,
}

#[derive(Debug, Deserialize)]
struct RawLinear {
    api_token: SecretSource,
    webhook_secret: SecretSource,
    assignee: String,
    #[serde(default)]
    admit_states: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawWorkflow {
    path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct RawServer {
    #[serde(default)]
    bind: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDebug {
    #[serde(default)]
    dir: Option<PathBuf>,
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPermissions {
    strategy: RawStrategy,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawStrategy {
    SettingsAllowlist,
    /// Accepts the shorthand `dangerously-skip` documented in
    /// `docs/examples/roki.annotated.toml` line 77.
    DangerouslySkip,
    DangerouslySkipPermissions,
}

impl From<RawStrategy> for PermissionStrategy {
    fn from(raw: RawStrategy) -> Self {
        match raw {
            RawStrategy::SettingsAllowlist => Self::SettingsAllowlist,
            RawStrategy::DangerouslySkip | RawStrategy::DangerouslySkipPermissions => {
                Self::DangerouslySkipPermissions
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

impl Config {
    /// Read and parse a `roki.toml` from disk.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        let body = fs::read_to_string(path).map_err(|source| ConfigError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        Self::load_from_str(&body)
    }

    /// Parse and validate the config body. The legacy-key sweep runs against
    /// the raw `toml::Value` tree before strongly-typed deserialization so the
    /// error names the offending namespace even when the typed struct would
    /// otherwise discard it via `#[serde(default)]`.
    pub fn load_from_str(body: &str) -> Result<Self, ConfigError> {
        let value: Value = toml::from_str(body).map_err(|e| ConfigError::Parse(e.to_string()))?;
        reject_legacy_keys(&value)?;

        let raw: RawConfig =
            toml::from_str(body).map_err(|e| ConfigError::Parse(e.to_string()))?;

        let raw_linear = raw.linear.ok_or(ConfigError::MissingBlock { block: "linear" })?;
        let raw_workflow = raw
            .workflow
            .ok_or(ConfigError::MissingBlock { block: "workflow" })?;
        let raw_permissions = raw
            .permissions
            .ok_or(ConfigError::MissingBlock { block: "permissions" })?;

        if raw_linear.assignee.trim().is_empty() {
            return Err(ConfigError::EmptyAssignee);
        }
        let assignee = if raw_linear.assignee == "me" {
            AssigneeSpec::Me
        } else {
            AssigneeSpec::Selector(raw_linear.assignee)
        };

        let admit_states: BTreeSet<String> = match raw_linear.admit_states {
            Some(values) if !values.is_empty() => values.into_iter().collect(),
            Some(_) => return Err(ConfigError::EmptyAdmitStates),
            // Default per Req 2.10 — applied here so empty-set rejection only fires
            // when the operator explicitly listed an empty array.
            None => BTreeSet::from(["Todo".to_owned()]),
        };
        if admit_states.is_empty() {
            return Err(ConfigError::EmptyAdmitStates);
        }

        if let Some(dup) = repos::find_duplicate_ghq(&raw.repos) {
            return Err(ConfigError::DuplicateRepo {
                ghq: dup.ghq,
                first: dup.first_index,
                second: dup.second_index,
            });
        }

        Ok(Self {
            linear: LinearConfig {
                api_token: raw_linear.api_token,
                webhook_secret: raw_linear.webhook_secret,
                assignee,
                admit_states,
            },
            workflow: WorkflowConfig {
                path: raw_workflow.path,
            },
            server: raw.server.map(|s| ServerConfig { bind: s.bind, port: s.port }).unwrap_or_default(),
            debug: raw
                .debug
                .map(|d| DebugConfig { dir: d.dir, level: d.level })
                .unwrap_or_default(),
            permissions: PermissionsConfig {
                strategy: raw_permissions.strategy.into(),
            },
            repos: raw.repos,
        })
    }

    /// Surface a refusal if `[workflow].path` is missing or unreadable per
    /// Req 2.4. Kept separate from `load_from_str` so unit tests of the
    /// parser do not need to materialize a real file on disk.
    pub fn validate_workflow_readable(&self) -> Result<(), ConfigError> {
        fs::metadata(&self.workflow.path).map_err(|source| ConfigError::WorkflowUnreadable {
            path: self.workflow.path.clone(),
            source,
        })?;
        // metadata check alone passes on a write-only file. Round-trip an open
        // so the operator finds out at startup, not at first read.
        fs::File::open(&self.workflow.path).map_err(|source| ConfigError::WorkflowUnreadable {
            path: self.workflow.path.clone(),
            source,
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Legacy-key sweep
// ---------------------------------------------------------------------------

/// Refuses (Req 2.12):
/// - `[judge].model`
/// - `[extension.linear_updater]` and any sub-key (`extension.linear_updater.*`)
/// - `[extension.gates.*]` (e.g. `extension.gates.spec`, `extension.gates.review`)
/// - `[extension.distill]` and any sub-key
fn reject_legacy_keys(value: &Value) -> Result<(), ConfigError> {
    let table = match value.as_table() {
        Some(t) => t,
        None => return Ok(()),
    };

    if let Some(judge) = table.get("judge")
        && let Some(judge_table) = judge.as_table()
        && judge_table.contains_key("model")
    {
        return Err(ConfigError::LegacyKey {
            key: "[judge].model".to_owned(),
        });
    }

    if let Some(extension) = table.get("extension").and_then(Value::as_table) {
        if extension.contains_key("linear_updater") {
            return Err(ConfigError::LegacyKey {
                key: "extension.linear_updater".to_owned(),
            });
        }
        if extension.contains_key("gates") {
            return Err(ConfigError::LegacyKey {
                key: "extension.gates".to_owned(),
            });
        }
        if extension.contains_key("distill") {
            return Err(ConfigError::LegacyKey {
                key: "extension.distill".to_owned(),
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn write_workflow(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("WORKFLOW.md");
        fs::write(&path, "# workflow stub").unwrap();
        path
    }

    fn minimal_toml(workflow_path: &Path) -> String {
        format!(
            r#"
[linear]
api_token = {{ env = "LINEAR_API_TOKEN" }}
webhook_secret = {{ env = "LINEAR_WEBHOOK_SECRET" }}
assignee = "me"

[workflow]
path = "{}"

[server]
bind = "127.0.0.1"
port = 8080

[permissions]
strategy = "settings-allowlist"
"#,
            workflow_path.display()
        )
    }

    #[test]
    fn loads_minimal_valid_config() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = minimal_toml(&workflow);
        let cfg = Config::load_from_str(&body).expect("minimal config must parse");

        assert!(matches!(cfg.linear.assignee, AssigneeSpec::Me));
        assert_eq!(cfg.linear.admit_states, BTreeSet::from(["Todo".to_owned()]));
        assert!(matches!(cfg.linear.api_token, SecretSource::Env(ref v) if v == "LINEAR_API_TOKEN"));
        assert!(matches!(cfg.linear.webhook_secret, SecretSource::Env(_)));
        assert_eq!(cfg.workflow.path, workflow);
        assert_eq!(cfg.server.bind.as_deref(), Some("127.0.0.1"));
        assert_eq!(cfg.server.port, Some(8080));
        assert!(matches!(cfg.permissions.strategy, PermissionStrategy::SettingsAllowlist));
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn defaults_admit_states_to_todo_when_omitted() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = minimal_toml(&workflow);
        let cfg = Config::load_from_str(&body).unwrap();
        assert_eq!(cfg.linear.admit_states, BTreeSet::from(["Todo".to_owned()]));
    }

    #[test]
    fn refuses_empty_assignee() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = ""

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::EmptyAssignee), "got {msg}");
        assert!(msg.contains("[linear].assignee"), "{msg}");
    }

    #[test]
    fn refuses_empty_admit_states() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"
admit_states = []

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::EmptyAdmitStates));
        assert!(msg.contains("[linear].admit_states"), "{msg}");
    }

    #[test]
    fn refuses_duplicate_ghq_identifier() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"

[[repos]]
ghq = "github.com/owner/repo"

[[repos]]
ghq = "github.com/owner/other"

[[repos]]
ghq = "github.com/owner/repo"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        let msg = err.to_string();
        match err {
            ConfigError::DuplicateRepo { ghq, first, second } => {
                assert_eq!(ghq, "github.com/owner/repo");
                assert_eq!(first, 0);
                assert_eq!(second, 2);
            }
            other => panic!("expected DuplicateRepo, got {other:?}"),
        }
        assert!(msg.contains("github.com/owner/repo"));
        assert!(msg.contains('0') && msg.contains('2'));
    }

    #[test]
    fn refuses_legacy_judge_model() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[judge]
model = "claude-sonnet-4"

[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::LegacyKey { ref key } if key == "[judge].model"));
        assert!(msg.contains("[judge].model"), "{msg}");
        assert!(
            msg.contains("orchestrator"),
            "legacy-key error must point at orchestrator-session migration: {msg}"
        );
    }

    #[test]
    fn refuses_legacy_extension_linear_updater() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[extension.linear_updater]
mode = "agent"

[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        assert!(
            matches!(err, ConfigError::LegacyKey { ref key } if key == "extension.linear_updater")
        );
    }

    #[test]
    fn refuses_legacy_extension_gates() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[extension.gates.spec]
enabled = true

[extension.gates.review]
enabled = true

[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        assert!(matches!(err, ConfigError::LegacyKey { ref key } if key == "extension.gates"));
    }

    #[test]
    fn refuses_legacy_extension_distill() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[extension.distill]
enabled = false

[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        assert!(matches!(err, ConfigError::LegacyKey { ref key } if key == "extension.distill"));
    }

    #[test]
    fn accepts_assignee_me_literal() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = minimal_toml(&workflow);
        let cfg = Config::load_from_str(&body).unwrap();
        // No Linear API call must happen at load time — `me` is stored verbatim
        // and resolved later by the runtime layer (task 3.4).
        assert!(matches!(cfg.linear.assignee, AssigneeSpec::Me));
        assert_eq!(cfg.linear.assignee.raw(), "me");
    }

    #[test]
    fn accepts_explicit_assignee_selector() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "user-uuid-or-email"

[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let cfg = Config::load_from_str(&body).unwrap();
        match cfg.linear.assignee {
            AssigneeSpec::Selector(value) => assert_eq!(value, "user-uuid-or-email"),
            other => panic!("expected Selector, got {other:?}"),
        }
    }

    #[test]
    fn secret_resolution_env_success() {
        let env = StaticEnv::new().set("LINEAR_API_TOKEN", "lin_api_xyz");
        let source = SecretSource::Env("LINEAR_API_TOKEN".to_owned());
        let resolved = source.resolve(&env).unwrap();
        assert_eq!(resolved.expose_secret(), "lin_api_xyz");
    }

    #[test]
    fn secret_resolution_env_missing() {
        let env = StaticEnv::new();
        let source = SecretSource::Env("LINEAR_API_TOKEN".to_owned());
        let err = source.resolve(&env).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::EnvVarMissing { ref var } if var == "LINEAR_API_TOKEN"));
        assert!(msg.contains("LINEAR_API_TOKEN"), "{msg}");
    }

    #[test]
    fn secret_resolution_file_success() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "lin_api_from_file").unwrap();
        let env = StaticEnv::new();
        let source = SecretSource::File(file.path().to_path_buf());
        let resolved = source.resolve(&env).unwrap();
        // Newline trimming keeps the secret stable when operators write the file
        // with a trailing `\n`, the default for most editors.
        assert_eq!(resolved.expose_secret(), "lin_api_from_file");
    }

    #[test]
    fn secret_resolution_file_missing() {
        let env = StaticEnv::new();
        let source = SecretSource::File(PathBuf::from("/nonexistent/roki/secret"));
        let err = source.resolve(&env).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::SecretFileUnreadable { .. }));
        assert!(msg.contains("/nonexistent/roki/secret"), "{msg}");
    }

    #[test]
    fn secret_value_redacts_in_debug_and_display() {
        let value = SecretValue::new("super-secret");
        assert_eq!(format!("{value}"), "***REDACTED***");
        assert_eq!(format!("{value:?}"), "SecretValue(***REDACTED***)");
        assert!(!format!("{value}").contains("super-secret"));
        assert!(!format!("{value:?}").contains("super-secret"));
        assert_eq!(value.expose_secret(), "super-secret");
    }

    #[test]
    fn validates_workflow_readable_ok_for_temp_file() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let cfg = Config::load_from_str(&minimal_toml(&workflow)).unwrap();
        cfg.validate_workflow_readable().expect("readable workflow");
    }

    #[test]
    fn validates_workflow_readable_refuses_missing_path() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let mut cfg = Config::load_from_str(&minimal_toml(&workflow)).unwrap();
        cfg.workflow.path = PathBuf::from("/nonexistent/WORKFLOW.md");
        let err = cfg.validate_workflow_readable().unwrap_err();
        assert!(matches!(err, ConfigError::WorkflowUnreadable { .. }));
        assert!(err.to_string().contains("/nonexistent/WORKFLOW.md"));
    }

    #[test]
    fn malformed_toml_names_offending_field() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[server]
port = "nine"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ConfigError::Parse(_)));
        assert!(msg.contains("port"), "parse error must name `port`: {msg}");
    }

    #[test]
    fn refuses_missing_linear_block() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[workflow]
path = "{}"

[permissions]
strategy = "settings-allowlist"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        assert!(matches!(err, ConfigError::MissingBlock { block: "linear" }));
    }

    #[test]
    fn refuses_missing_workflow_block() {
        let body = r#"
[linear]
api_token = { env = "X" }
webhook_secret = { env = "Y" }
assignee = "me"

[permissions]
strategy = "settings-allowlist"
"#;
        let err = Config::load_from_str(body).unwrap_err();
        assert!(matches!(err, ConfigError::MissingBlock { block: "workflow" }));
    }

    #[test]
    fn refuses_missing_permissions_block() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"
"#,
            workflow.display()
        );
        let err = Config::load_from_str(&body).unwrap_err();
        assert!(matches!(err, ConfigError::MissingBlock { block: "permissions" }));
    }

    #[test]
    fn empty_repos_allowlist_is_accepted_with_warning_path() {
        // Req 2.7: empty allowlist must not refuse startup. The warning is logged
        // at the daemon-bootstrap layer; this loader simply yields an empty
        // `repos` vector.
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let cfg = Config::load_from_str(&minimal_toml(&workflow)).unwrap();
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn permissions_strategy_accepts_dangerously_skip() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let body = format!(
            r#"
[linear]
api_token = {{ env = "X" }}
webhook_secret = {{ env = "Y" }}
assignee = "me"

[workflow]
path = "{}"

[permissions]
strategy = "dangerously-skip"
"#,
            workflow.display()
        );
        let cfg = Config::load_from_str(&body).unwrap();
        assert!(matches!(
            cfg.permissions.strategy,
            PermissionStrategy::DangerouslySkipPermissions
        ));
    }

    #[test]
    fn load_from_path_round_trips_disk() {
        let dir = TempDir::new().unwrap();
        let workflow = write_workflow(&dir);
        let toml_path = dir.path().join("roki.toml");
        fs::write(&toml_path, minimal_toml(&workflow)).unwrap();
        let cfg = Config::load_from_path(&toml_path).expect("load_from_path must succeed");
        assert!(matches!(cfg.linear.assignee, AssigneeSpec::Me));
    }

    #[test]
    fn load_from_path_reports_missing_file() {
        let err = Config::load_from_path(Path::new("/nonexistent/roki.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::ReadFile { .. }));
    }
}
