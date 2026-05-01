//! Layered configuration loader for the roki daemon.
//!
//! This module implements task 1.2 of the roki-mvp spec. It owns:
//!
//! * the configuration struct hierarchy (root `Config` plus per-repo
//!   `RepoConfig` entries);
//! * loading from a TOML file plus environment overrides;
//! * structured validation that names the offending field on failure
//!   (Requirement 1.2);
//! * explicit refusal when the Linear API token is missing
//!   (Requirement 2.5);
//! * explicit refusal when no permission strategy is configured
//!   (Requirement 9.5).
//!
//! The Linear API token is wrapped in [`SecretString`], whose `Debug` impl
//! redacts the value so it never leaks through tracing or panic output.
//! Logging-layer redaction is task 1.3's concern; here we only ensure the
//! token cannot accidentally be formatted into a log line via `Debug`.

pub mod repos;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use repos::{GhqIdentifierError, RepoConfig, validate_ghq_identifier};

/// Default polling cadence cap (Requirement 3.2: <= 5 min per scope).
pub const DEFAULT_POLLING_CADENCE_SECONDS: u64 = 300;

/// Hard upper bound on the polling cadence (5 minutes per Requirement 3.2).
pub const MAX_POLLING_CADENCE_SECONDS: u64 = 300;

/// Default for the operator-configurable max-concurrent-workers knob
/// (design.md "Performance & Scalability": "defaulted to a small integer").
pub const DEFAULT_MAX_CONCURRENT_WORKERS: u32 = 4;

/// Default environment variable name for the Linear API token.
pub const DEFAULT_LINEAR_TOKEN_ENV: &str = "LINEAR_API_TOKEN";

/// Default loopback bind address for the daemon's HTTP surface.
/// SPEC.md §3.2: the operator opts into wider exposure explicitly.
pub const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1";

/// Default port for the daemon's HTTP surface.
pub const DEFAULT_BIND_PORT: u16 = 7878;

/// Root configuration for the roki daemon.
///
/// Field-level documentation lists the requirement each field traces to so
/// validation errors stay anchored to the spec.
///
/// Per task 6.1 the per-daemon `workspace_root` was removed: workspaces are
/// now git worktrees laid out next to the source repo
/// (`{repo_path}/../{repo_name}.{branch_sanitized}`), and `repo_path` is
/// resolved at runtime via `ghq` from the per-repo identifier.
#[derive(Debug, Clone)]
pub struct Config {
    /// Resolved Linear API token. Requirement 2.5.
    pub linear_token: SecretString,

    /// Minimum interval between polls for the same Linear scope.
    /// Requirement 3.2 caps this at 5 minutes.
    pub polling_cadence: Duration,

    /// Maximum number of concurrent active worker subprocesses.
    pub max_concurrent_workers: u32,

    /// Permission strategy to apply at worker launch. Requirement 9.5.
    pub permission_strategy: PermissionStrategy,

    /// Per-repo configuration. Requirement 2.1.
    pub repos: Vec<RepoConfig>,

    /// HTTP server bind address. Defaults to [`DEFAULT_BIND_ADDRESS`].
    /// SPEC.md §3.2 / task 5.1.
    pub server_bind: IpAddr,

    /// HTTP server port. Defaults to [`DEFAULT_BIND_PORT`].
    /// SPEC.md §3.2 / task 5.1.
    pub server_port: u16,

    /// Optional override for the `claude` binary path. When `None` the
    /// bootstrap resolves `claude` via `$PATH` discovery.
    /// SPEC.md §3.2 / task 5.1.
    pub claude_binary: Option<PathBuf>,

    /// Optional Linear GraphQL endpoint override. `None` means production
    /// (`https://api.linear.app/graphql`); tests set this to a wiremock URL.
    /// Not pinned in SPEC.md — purely an additive runtime knob.
    pub linear_endpoint: Option<String>,

    /// Workspace-level Linear configuration (Requirement 2.3, post-7.1a).
    /// Carries the env-var name resolving to the single workspace-level
    /// webhook HMAC secret and the optional `token_env` source the
    /// `linear_token` was resolved from.
    pub linear: LinearConfig,

    /// Workspace-level workflow policy configuration (Requirement 2.4,
    /// post-7.1a). The single `WORKFLOW.md` policy applies regardless of
    /// which configured repo(s) the agent picks.
    pub workflow: WorkflowConfig,
}

/// Resolved workspace-level Linear configuration (post-7.1a).
///
/// `webhook_secret_env` is the env-var name the bootstrap reads to obtain
/// the single workspace-level HMAC secret. `token_env` is preserved for
/// observability — the actual token is wrapped in [`SecretString`] on
/// [`Config::linear_token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearConfig {
    /// Env-var name that holds the Linear API token. Defaults to
    /// [`DEFAULT_LINEAR_TOKEN_ENV`] when not configured.
    pub token_env: String,

    /// Env-var name that holds the workspace-level webhook HMAC secret.
    /// Required (Requirement 2.3); the bootstrap resolves the actual
    /// secret value at startup.
    pub webhook_secret_env: String,
}

/// Resolved workspace-level workflow configuration (post-7.1a).
///
/// One `WORKFLOW.md` policy applies regardless of which configured repo(s)
/// the agent operates in (Requirement 2.4, locked decision #6 in
/// `design-agent-driven-repo-selection.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowConfig {
    /// Filesystem path to the workspace-level `WORKFLOW.md`. Required.
    pub path: PathBuf,
}

/// On-disk shape of the config file.
///
/// We deserialize this and then translate it (with environment overrides and
/// explicit token resolution) into the runtime [`Config`]. Keeping this type
/// separate from `Config` lets the loader give precise field-level errors
/// without serde's default messages leaking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    linear: Option<LinearFile>,

    #[serde(default)]
    polling_cadence_seconds: Option<u64>,

    #[serde(default)]
    max_concurrent_workers: Option<u32>,

    #[serde(default)]
    permissions: Option<PermissionsFile>,

    #[serde(default)]
    repos: Vec<RepoConfig>,

    #[serde(default)]
    server: Option<ServerFile>,

    #[serde(default)]
    claude_binary: Option<PathBuf>,

    #[serde(default)]
    workflow: Option<WorkflowFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowFile {
    /// Path to the workspace-level `WORKFLOW.md` policy file. Required.
    #[serde(default)]
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    #[serde(default)]
    bind: Option<String>,

    #[serde(default)]
    port: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinearFile {
    /// Environment variable to read the API token from.
    #[serde(default)]
    token_env: Option<String>,

    /// Path to a file containing the API token (single-line UTF-8).
    #[serde(default)]
    token_file: Option<PathBuf>,

    /// Optional endpoint override. Production callers leave this absent so
    /// the daemon hits `api.linear.app/graphql`. Integration tests set this
    /// to a wiremock URL so the tracker never touches the real Linear API.
    #[serde(default)]
    endpoint: Option<String>,

    /// Env-var name that holds the workspace-level webhook HMAC secret.
    /// Required (Requirement 2.3, post-7.1a).
    #[serde(default)]
    webhook_secret_env: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionsFile {
    #[serde(default)]
    strategy: Option<PermissionStrategyKind>,

    /// Path to the Claude Code `--settings` allowlist file. Required when the
    /// strategy is `allowlist`.
    #[serde(default)]
    settings: Option<PathBuf>,
}

/// Permission strategy applied at worker launch (Requirement 9.3, 9.4, 9.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionStrategy {
    /// `--settings` allowlist; the path points at a Claude Code settings file.
    Allowlist { settings_path: PathBuf },
    /// `--dangerously-skip-permissions` fallback.
    DangerouslySkipPermissions,
}

/// Tag used to select between the two permission strategy variants in the
/// config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PermissionStrategyKind {
    Allowlist,
    DangerouslySkipPermissions,
}

/// Newtype wrapping the Linear API token so its value cannot leak through
/// `Debug` output (e.g., panic messages, ad-hoc `tracing::debug!` of a config
/// struct). Logging-layer redaction is a separate concern handled in task 1.3.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap an owned token. The constructor is the only way to put a value
    /// in; reading it back requires the explicit [`Self::expose`] method.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Expose the raw secret. Call sites that invoke this are auditable.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(***)")
    }
}

/// Environment overrides applied after the file is read.
///
/// Carrying overrides through a struct (rather than reading `std::env`
/// directly inside the loader) keeps loading deterministic for tests:
/// callers stitch their own env shape together.
#[derive(Debug, Clone, Default)]
pub struct EnvOverrides {
    /// Token literal from environment; takes precedence over `token_env` /
    /// `token_file` declared in the config file.
    pub linear_token: Option<String>,

    /// Override the polling cadence (seconds).
    pub polling_cadence_seconds: Option<u64>,

    /// Override the max-concurrent-workers knob.
    pub max_concurrent_workers: Option<u32>,
}

impl EnvOverrides {
    /// Read overrides from the process environment. Variables:
    ///
    /// * `ROKI_LINEAR_TOKEN` — literal token (highest precedence).
    /// * `ROKI_POLLING_CADENCE_SECONDS` — polling cadence override.
    /// * `ROKI_MAX_CONCURRENT_WORKERS` — concurrency override.
    ///
    /// `ROKI_WORKSPACE_ROOT` was removed in task 6.1 along with the
    /// daemon-level workspace root: workspaces are now git worktrees laid out
    /// next to each repo. Operators upgrading from the sandbox-dir model
    /// must drop the env var; the daemon ignores it.
    pub fn from_process_env() -> Result<Self, ConfigError> {
        let linear_token = env::var("ROKI_LINEAR_TOKEN").ok();
        let polling_cadence_seconds = parse_env_u64("ROKI_POLLING_CADENCE_SECONDS")?;
        let max_concurrent_workers = parse_env_u32("ROKI_MAX_CONCURRENT_WORKERS")?;
        Ok(Self {
            linear_token,
            polling_cadence_seconds,
            max_concurrent_workers,
        })
    }
}

fn parse_env_u64(var: &str) -> Result<Option<u64>, ConfigError> {
    match env::var(var) {
        Ok(raw) => raw
            .parse::<u64>()
            .map(Some)
            .map_err(|_| ConfigError::InvalidField {
                field: var.to_string(),
                reason: format!("expected unsigned integer, got `{raw}`"),
            }),
        Err(_) => Ok(None),
    }
}

fn parse_env_u32(var: &str) -> Result<Option<u32>, ConfigError> {
    match env::var(var) {
        Ok(raw) => raw
            .parse::<u32>()
            .map(Some)
            .map_err(|_| ConfigError::InvalidField {
                field: var.to_string(),
                reason: format!("expected unsigned integer, got `{raw}`"),
            }),
        Err(_) => Ok(None),
    }
}

/// Structured configuration error.
///
/// Every variant carries the offending field path so log entries can name the
/// failure (Requirement 1.2). The `Display` impl always includes the field
/// name so callers do not have to pattern-match to produce a usable message.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config file `{path}` (field `{field}`): {reason}")]
    Parse {
        path: PathBuf,
        field: String,
        reason: String,
    },

    #[error("config field `{field}` is invalid: {reason}")]
    InvalidField { field: String, reason: String },

    #[error("config field `{field}` is required but missing")]
    MissingField { field: String },

    #[error(
        "Linear API token missing: set `{field}` (env var override) or configure \
         `linear.token_env` / `linear.token_file` in the config file"
    )]
    MissingLinearToken { field: String },

    #[error(
        "permission strategy missing: set `permissions.strategy` to `allowlist` \
         (with `permissions.settings`) or `dangerously_skip_permissions`"
    )]
    MissingPermissionStrategy,
}

impl ConfigError {
    /// Return the `field` path reported by this error, when one applies.
    ///
    /// Used by the unit tests that prove the malformed-config case names the
    /// failing field (the observable-completion criterion of task 1.2).
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::Parse { field, .. }
            | Self::InvalidField { field, .. }
            | Self::MissingField { field }
            | Self::MissingLinearToken { field } => Some(field),
            Self::MissingPermissionStrategy => Some("permissions.strategy"),
            Self::Io { .. } => None,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file plus the supplied environment
    /// overrides.
    ///
    /// The order of precedence is: env override > config file value >
    /// documented default. The Linear API token has no default — it must come
    /// from the env override, the file's `linear.token_env`, or the file's
    /// `linear.token_file` (Requirement 2.5).
    pub fn load(path: &Path, env: &EnvOverrides) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::load_from_str(&raw, path, env)
    }

    /// Load configuration from an in-memory TOML string (used by unit tests
    /// and by [`Self::load`]).
    pub fn load_from_str(
        raw: &str,
        source_path: &Path,
        env: &EnvOverrides,
    ) -> Result<Self, ConfigError> {
        let file: ConfigFile = toml::from_str(raw).map_err(|err| {
            // toml's typed errors report a span pointing into the source. We
            // use the span to recover the offending key path so callers can
            // log the failing field directly (Requirement 1.2).
            let field = field_from_toml_error(&err, raw).unwrap_or_else(|| "<root>".to_string());
            ConfigError::Parse {
                path: source_path.to_path_buf(),
                field,
                reason: err.message().to_string(),
            }
        })?;
        Self::assemble(file, env)
    }

    fn assemble(file: ConfigFile, env: &EnvOverrides) -> Result<Self, ConfigError> {
        let linear_token = resolve_linear_token(env, file.linear.as_ref())?;

        let polling_cadence_seconds = env
            .polling_cadence_seconds
            .or(file.polling_cadence_seconds)
            .unwrap_or(DEFAULT_POLLING_CADENCE_SECONDS);
        if polling_cadence_seconds == 0 {
            return Err(ConfigError::InvalidField {
                field: "polling_cadence_seconds".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }
        if MAX_POLLING_CADENCE_SECONDS < polling_cadence_seconds {
            return Err(ConfigError::InvalidField {
                field: "polling_cadence_seconds".to_string(),
                reason: format!(
                    "must not exceed {MAX_POLLING_CADENCE_SECONDS} seconds (Linear cadence cap)"
                ),
            });
        }

        let max_concurrent_workers = env
            .max_concurrent_workers
            .or(file.max_concurrent_workers)
            .unwrap_or(DEFAULT_MAX_CONCURRENT_WORKERS);
        if max_concurrent_workers == 0 {
            return Err(ConfigError::InvalidField {
                field: "max_concurrent_workers".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }

        let permission_strategy = resolve_permission_strategy(file.permissions.as_ref())?;

        validate_repos(&file.repos)?;

        let (server_bind, server_port) = resolve_server(file.server.as_ref())?;

        let linear_endpoint = file.linear.as_ref().and_then(|f| f.endpoint.clone());
        let linear = resolve_linear_block(file.linear.as_ref())?;
        let workflow = resolve_workflow_block(file.workflow.as_ref())?;

        Ok(Self {
            linear_token,
            polling_cadence: Duration::from_secs(polling_cadence_seconds),
            max_concurrent_workers,
            permission_strategy,
            repos: file.repos,
            server_bind,
            server_port,
            claude_binary: file.claude_binary,
            linear_endpoint,
            linear,
            workflow,
        })
    }
}

/// Resolve the workspace-level `[linear]` block. Per Requirement 2.3 the
/// webhook secret env-var name is required; absence is a hard refusal that
/// names the offending field.
fn resolve_linear_block(linear: Option<&LinearFile>) -> Result<LinearConfig, ConfigError> {
    let Some(linear) = linear else {
        return Err(ConfigError::MissingField {
            field: "linear.webhook_secret_env".to_string(),
        });
    };
    let Some(webhook_secret_env) = linear.webhook_secret_env.as_deref() else {
        return Err(ConfigError::MissingField {
            field: "linear.webhook_secret_env".to_string(),
        });
    };
    if webhook_secret_env.trim().is_empty() {
        return Err(ConfigError::InvalidField {
            field: "linear.webhook_secret_env".to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    let token_env = linear
        .token_env
        .clone()
        .unwrap_or_else(|| DEFAULT_LINEAR_TOKEN_ENV.to_string());
    Ok(LinearConfig {
        token_env,
        webhook_secret_env: webhook_secret_env.to_string(),
    })
}

/// Resolve the workspace-level `[workflow]` block. Per Requirement 2.4 the
/// path is required; absence is a hard refusal.
fn resolve_workflow_block(workflow: Option<&WorkflowFile>) -> Result<WorkflowConfig, ConfigError> {
    let Some(workflow) = workflow else {
        return Err(ConfigError::MissingField {
            field: "workflow.path".to_string(),
        });
    };
    let Some(path) = workflow.path.as_ref() else {
        return Err(ConfigError::MissingField {
            field: "workflow.path".to_string(),
        });
    };
    if path.as_os_str().is_empty() {
        return Err(ConfigError::InvalidField {
            field: "workflow.path".to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    Ok(WorkflowConfig { path: path.clone() })
}

fn resolve_server(server: Option<&ServerFile>) -> Result<(IpAddr, u16), ConfigError> {
    let default_bind: IpAddr =
        DEFAULT_BIND_ADDRESS
            .parse()
            .map_err(|err: std::net::AddrParseError| ConfigError::InvalidField {
                field: "server.bind".to_string(),
                reason: format!(
                    "default bind address `{DEFAULT_BIND_ADDRESS}` is malformed: {err}"
                ),
            })?;

    let Some(server) = server else {
        return Ok((default_bind, DEFAULT_BIND_PORT));
    };

    let bind = match server.bind.as_deref() {
        Some(raw) => raw
            .parse::<IpAddr>()
            .map_err(|err| ConfigError::InvalidField {
                field: "server.bind".to_string(),
                reason: format!("expected an IP address, got `{raw}`: {err}"),
            })?,
        None => default_bind,
    };
    let port = server.port.unwrap_or(DEFAULT_BIND_PORT);
    if port == 0 {
        return Err(ConfigError::InvalidField {
            field: "server.port".to_string(),
            reason: "must be greater than zero".to_string(),
        });
    }
    Ok((bind, port))
}

fn resolve_linear_token(
    env: &EnvOverrides,
    linear: Option<&LinearFile>,
) -> Result<SecretString, ConfigError> {
    if let Some(token) = env.linear_token.as_ref() {
        if token.trim().is_empty() {
            return Err(ConfigError::InvalidField {
                field: "ROKI_LINEAR_TOKEN".to_string(),
                reason: "token must not be empty".to_string(),
            });
        }
        return Ok(SecretString::new(token.clone()));
    }

    let Some(linear) = linear else {
        return Err(ConfigError::MissingLinearToken {
            field: "ROKI_LINEAR_TOKEN".to_string(),
        });
    };

    if let Some(env_var) = linear.token_env.as_deref() {
        return read_token_from_env(env_var);
    }

    if let Some(path) = linear.token_file.as_deref() {
        return read_token_from_file(path);
    }

    Err(ConfigError::MissingLinearToken {
        field: "linear.token_env".to_string(),
    })
}

fn read_token_from_env(var: &str) -> Result<SecretString, ConfigError> {
    let value = env::var(var).map_err(|_| ConfigError::MissingLinearToken {
        field: format!("linear.token_env ({var})"),
    })?;
    if value.trim().is_empty() {
        return Err(ConfigError::InvalidField {
            field: format!("linear.token_env ({var})"),
            reason: "token must not be empty".to_string(),
        });
    }
    Ok(SecretString::new(value))
}

fn read_token_from_file(path: &Path) -> Result<SecretString, ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidField {
            field: "linear.token_file".to_string(),
            reason: format!("file `{}` is empty", path.display()),
        });
    }
    Ok(SecretString::new(trimmed.to_string()))
}

fn resolve_permission_strategy(
    permissions: Option<&PermissionsFile>,
) -> Result<PermissionStrategy, ConfigError> {
    let Some(permissions) = permissions else {
        return Err(ConfigError::MissingPermissionStrategy);
    };

    let Some(kind) = permissions.strategy else {
        return Err(ConfigError::MissingPermissionStrategy);
    };

    match kind {
        PermissionStrategyKind::Allowlist => {
            let settings_path =
                permissions
                    .settings
                    .clone()
                    .ok_or_else(|| ConfigError::MissingField {
                        field: "permissions.settings".to_string(),
                    })?;
            Ok(PermissionStrategy::Allowlist { settings_path })
        }
        PermissionStrategyKind::DangerouslySkipPermissions => {
            Ok(PermissionStrategy::DangerouslySkipPermissions)
        }
    }
}

/// Validate the per-repo allowlist (Requirement 2.1) and refuse duplicates
/// (Requirement 2.2, post-7.1a).
fn validate_repos(repos: &[RepoConfig]) -> Result<(), ConfigError> {
    let mut seen: HashMap<&str, usize> = HashMap::with_capacity(repos.len());
    for (index, repo) in repos.iter().enumerate() {
        if let Err(err) = validate_ghq_identifier(&repo.repo) {
            return Err(ConfigError::InvalidField {
                field: format!("repos[{index}].repo"),
                reason: err.message().to_string(),
            });
        }
        if let Some(prior) = seen.insert(repo.repo.as_str(), index) {
            return Err(ConfigError::InvalidField {
                field: format!("repos[{index}].repo"),
                reason: format!(
                    "duplicate ghq identifier `{}` (also declared at repos[{prior}])",
                    repo.repo,
                ),
            });
        }
    }
    Ok(())
}

/// Best-effort extraction of the offending key from a `toml::de::Error`.
///
/// Strategy:
/// 1. The error message itself names the field for `unknown field` and
///    `missing field` cases (serde's `deny_unknown_fields` / typed missing).
/// 2. For typed value errors, the error carries a span pointing into the
///    source. We read the key name on that source line.
///
/// Returning `None` causes the loader to fall back to "<root>", so the field
/// is still named (Requirement 1.2 — error must identify the offending field).
fn field_from_toml_error(err: &toml::de::Error, source: &str) -> Option<String> {
    let rendered = err.message().to_string();
    if let Some(name) = extract_quoted_after(&rendered, "unknown field `", '`') {
        return Some(name);
    }
    if let Some(name) = extract_quoted_after(&rendered, "missing field `", '`') {
        return Some(name);
    }
    if let Some(span) = err.span() {
        return key_name_at_offset(source, span.start);
    }
    None
}

fn extract_quoted_after(haystack: &str, marker: &str, terminator: char) -> Option<String> {
    let start = haystack.find(marker)? + marker.len();
    let rest = &haystack[start..];
    let end = rest.find(terminator)?;
    Some(rest[..end].to_string())
}

/// Recover the TOML key name on the line that contains `byte_offset`.
///
/// TOML key/value lines have the shape `key = value` or `[table.key]`. We
/// extract the key segment by searching for the assignment or table delimiter
/// nearest the failure location. Whitespace and quoting are tolerated.
fn key_name_at_offset(source: &str, byte_offset: usize) -> Option<String> {
    // Find the line containing byte_offset.
    let line_start = source[..byte_offset.min(source.len())]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line_end = source[byte_offset.min(source.len())..]
        .find('\n')
        .map(|p| byte_offset + p)
        .unwrap_or(source.len());
    let line = source.get(line_start..line_end)?.trim();
    if line.is_empty() {
        return None;
    }
    // Table header: [section.key]
    if let Some(stripped) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return Some(stripped.trim().trim_matches('"').to_string());
    }
    // Inline key/value: key = value (or "quoted key" = value).
    let eq_pos = line.find('=')?;
    let key = line[..eq_pos].trim().trim_matches('"');
    if key.is_empty() {
        return None;
    }
    Some(key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        PathBuf::from("test-config.toml")
    }

    fn valid_config_toml() -> &'static str {
        // Post-7.1a shape: only `repo` per `[[repos]]`, workspace-level
        // `[linear].webhook_secret_env`, workspace-level `[workflow].path`.
        r#"
polling_cadence_seconds = 120
max_concurrent_workers = 3

[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "allowlist"
settings = "/etc/roki/claude-settings.json"

[[repos]]
repo = "owner/core"
"#
    }

    fn env_with_token() -> EnvOverrides {
        EnvOverrides {
            linear_token: Some("lin_api_test_secret".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn loads_a_valid_example_config() {
        let cfg = Config::load_from_str(valid_config_toml(), &fixture_path(), &env_with_token())
            .expect("valid config must load");

        assert_eq!(cfg.polling_cadence, Duration::from_secs(120));
        assert_eq!(cfg.max_concurrent_workers, 3);
        assert_eq!(cfg.linear_token.expose(), "lin_api_test_secret");
        assert!(matches!(
            cfg.permission_strategy,
            PermissionStrategy::Allowlist { .. }
        ));
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.repos[0].repo, "owner/core");
        assert_eq!(cfg.linear.webhook_secret_env, "ROKI_LINEAR_WEBHOOK_SECRET");
        assert_eq!(
            cfg.workflow.path.to_str().unwrap(),
            "/srv/policy/WORKFLOW.md"
        );
    }

    #[test]
    fn workspace_root_in_config_file_is_refused() {
        // Task 6.1: workspace_root was removed from the schema. An existing
        // config that still carries it must fail to load with the offending
        // key named (deny_unknown_fields surfaces this).
        let body = r#"
workspace_root = "/var/lib/roki/workspaces"

[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "dangerously_skip_permissions"
"#;
        let err = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect_err("legacy workspace_root must be refused");
        match err {
            ConfigError::Parse { ref field, .. } => {
                assert!(
                    field.contains("workspace_root"),
                    "expected error to name `workspace_root`, got `{field}`",
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn invalid_repo_ghq_identifier_is_refused() {
        let bad = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "/abs/path/to/local"
"#;
        let err = Config::load_from_str(bad, &fixture_path(), &env_with_token())
            .expect_err("absolute-path-shaped ghq identifier must be refused");
        assert_eq!(err.field(), Some("repos[0].repo"));
    }

    #[test]
    fn malformed_config_returns_error_naming_failing_field() {
        // `polling_cadence_seconds` is typed as a u64; a string here forces a
        // typed parse error whose offending field must be surfaced verbatim.
        let malformed = r#"
polling_cadence_seconds = "not-a-number"

[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "dangerously_skip_permissions"
"#;
        let err = Config::load_from_str(malformed, &fixture_path(), &env_with_token())
            .expect_err("malformed config must fail to load");

        let field = err
            .field()
            .expect("malformed-config error must name a failing field");
        assert!(
            field.contains("polling_cadence_seconds"),
            "error field `{field}` did not identify the offending key; full error: {err}"
        );
        // Defense in depth: the rendered message must also surface the field
        // name so log lines (Requirement 1.2) include it without further
        // pattern matching.
        assert!(
            err.to_string().contains("polling_cadence_seconds"),
            "rendered error did not name the offending field: {err}"
        );
    }

    #[test]
    fn missing_linear_token_is_refused() {
        // No env override AND no `[linear]` block: refuse to start
        // (Requirement 2.5).
        let no_token = r#"
[permissions]
strategy = "dangerously_skip_permissions"
"#;
        let err = Config::load_from_str(no_token, &fixture_path(), &EnvOverrides::default())
            .expect_err("missing token must be refused");
        assert!(matches!(err, ConfigError::MissingLinearToken { .. }));
    }

    #[test]
    fn empty_linear_token_in_env_override_is_refused() {
        let env = EnvOverrides {
            linear_token: Some("   ".to_string()),
            ..Default::default()
        };
        let err = Config::load_from_str(valid_config_toml(), &fixture_path(), &env)
            .expect_err("empty token must be refused");
        assert!(
            matches!(err, ConfigError::InvalidField { ref field, .. } if field == "ROKI_LINEAR_TOKEN")
        );
    }

    #[test]
    fn missing_permission_strategy_is_refused() {
        // Requirement 9.5: refuse to start when neither permission strategy
        // is configured.
        let no_perms = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
"#;
        let err = Config::load_from_str(no_perms, &fixture_path(), &env_with_token())
            .expect_err("missing permission strategy must be refused");
        assert!(matches!(err, ConfigError::MissingPermissionStrategy));
        assert_eq!(err.field(), Some("permissions.strategy"));
    }

    #[test]
    fn allowlist_without_settings_path_names_the_field() {
        let no_settings = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "allowlist"
"#;
        let err = Config::load_from_str(no_settings, &fixture_path(), &env_with_token())
            .expect_err("allowlist strategy without settings must be refused");
        assert_eq!(err.field(), Some("permissions.settings"));
    }

    #[test]
    fn polling_cadence_above_cap_is_rejected_by_field_name() {
        // Requirement 3.2 caps polling at 5 minutes per scope.
        let too_slow = r#"
polling_cadence_seconds = 600

[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "dangerously_skip_permissions"
"#;
        let err = Config::load_from_str(too_slow, &fixture_path(), &env_with_token())
            .expect_err("cadence above cap must be rejected");
        assert_eq!(err.field(), Some("polling_cadence_seconds"));
    }

    #[test]
    fn unknown_field_names_the_offending_key() {
        let unknown = r#"
unexpected_top_level = true

[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "dangerously_skip_permissions"
"#;
        let err = Config::load_from_str(unknown, &fixture_path(), &env_with_token())
            .expect_err("unknown field must be refused");
        match err {
            ConfigError::Parse { ref field, .. } => {
                assert!(
                    field.contains("unexpected_top_level"),
                    "expected error to name `unexpected_top_level`, got `{field}`"
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn env_override_supersedes_file_token_source() {
        let cfg = Config::load_from_str(valid_config_toml(), &fixture_path(), &env_with_token())
            .expect("valid config must load");
        // The file's [linear].token_env points at MY_LINEAR_TOKEN, but the
        // env override carries a literal token; the literal must win.
        assert_eq!(cfg.linear_token.expose(), "lin_api_test_secret");
    }

    #[test]
    fn secret_string_debug_does_not_leak_value() {
        let secret = SecretString::new("super-sensitive-token-value");
        let debug_repr = format!("{secret:?}");
        assert!(!debug_repr.contains("super-sensitive-token-value"));
        assert!(debug_repr.contains("***"));
    }

    #[test]
    fn token_loaded_from_file_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_path = dir.path().join("token.txt");
        std::fs::write(&token_path, "file-token-value\n").expect("write token");
        let toml_body = format!(
            r#"
[linear]
token_file = "{}"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"
"#,
            token_path.display()
        );
        let cfg = Config::load_from_str(&toml_body, &fixture_path(), &EnvOverrides::default())
            .expect("file-backed token must load");
        assert_eq!(cfg.linear_token.expose(), "file-token-value");
    }

    #[test]
    fn server_section_defaults_to_loopback_and_documented_port() {
        // SPEC.md §3.2 / task 5.1: when no `[server]` section is configured
        // the daemon binds to 127.0.0.1:7878 (loopback only — operator opts
        // into wider exposure explicitly).
        let cfg = Config::load_from_str(valid_config_toml(), &fixture_path(), &env_with_token())
            .expect("valid config without [server] must load");
        assert_eq!(cfg.server_bind.to_string(), "127.0.0.1");
        assert_eq!(cfg.server_port, 7878);
    }

    #[test]
    fn server_section_overrides_bind_and_port() {
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"

[server]
bind = "0.0.0.0"
port = 9090
"#;
        let cfg = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect("server overrides must load");
        assert_eq!(cfg.server_bind.to_string(), "0.0.0.0");
        assert_eq!(cfg.server_port, 9090);
    }

    #[test]
    fn server_section_rejects_malformed_bind_address() {
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "dangerously_skip_permissions"

[server]
bind = "not-an-ip"
"#;
        let err = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect_err("malformed bind must be refused");
        assert_eq!(err.field(), Some("server.bind"));
    }

    #[test]
    fn server_section_rejects_zero_port() {
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "dangerously_skip_permissions"

[server]
port = 0
"#;
        let err = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect_err("port=0 must be refused");
        assert_eq!(err.field(), Some("server.port"));
    }

    #[test]
    fn claude_binary_override_round_trips_through_config() {
        let body = r#"
claude_binary = "/usr/local/bin/claude-test"

[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"
"#;
        let cfg = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect("claude_binary override must load");
        assert_eq!(
            cfg.claude_binary.as_deref().map(|p| p.to_str().unwrap()),
            Some("/usr/local/bin/claude-test"),
        );
    }

    #[test]
    fn workspace_level_webhook_secret_env_round_trips() {
        // Post-7.1a: the per-repo `webhook_secret_env` was replaced by a
        // single workspace-level `[linear].webhook_secret_env`.
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "owner/core"
"#;
        let cfg = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect("workspace-level webhook_secret_env must round-trip");
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.repos[0].repo, "owner/core");
        assert_eq!(cfg.linear.webhook_secret_env, "ROKI_LINEAR_WEBHOOK_SECRET");
    }

    // ---- 7.1a: post-rewrite schema tests --------------------------------

    fn valid_config_toml_71a() -> &'static str {
        // The new shape after task 7.1a: `[[repos]]` carries only `repo`,
        // `[linear]` declares the workspace-level webhook secret env var, and
        // `[workflow]` declares the single workspace-level policy path.
        r#"
polling_cadence_seconds = 120
max_concurrent_workers = 3

[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "allowlist"
settings = "/etc/roki/claude-settings.json"

[[repos]]
repo = "owner/core"

[[repos]]
repo = "owner/infra"
"#
    }

    #[test]
    fn loads_a_valid_71a_shape_config() {
        // The post-7.1a schema parses end-to-end: only `repo` per entry,
        // workspace-level `[linear].webhook_secret_env` and `[workflow].path`.
        let cfg =
            Config::load_from_str(valid_config_toml_71a(), &fixture_path(), &env_with_token())
                .expect("post-7.1a config must load");

        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.repos[0].repo, "owner/core");
        assert_eq!(cfg.repos[1].repo, "owner/infra");
        assert_eq!(cfg.linear.webhook_secret_env, "ROKI_LINEAR_WEBHOOK_SECRET",);
        assert_eq!(
            cfg.workflow.path.to_str().unwrap(),
            "/srv/policy/WORKFLOW.md"
        );
    }

    #[test]
    fn linear_block_requires_webhook_secret_env() {
        // Requirement 2.3: refuse to start if the workspace-level webhook
        // secret source is absent.
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "owner/core"
"#;
        let err = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect_err("missing webhook_secret_env must be refused");
        assert_eq!(err.field(), Some("linear.webhook_secret_env"));
    }

    #[test]
    fn workflow_block_is_required() {
        // Requirement 2.4: refuse to start if the workspace-level workflow
        // path is absent.
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "owner/core"
"#;
        let err = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect_err("missing [workflow] block must be refused");
        assert_eq!(err.field(), Some("workflow.path"));
    }

    #[test]
    fn duplicate_repo_entries_are_refused() {
        // Requirement 2.2: duplicate `[[repos]]` entries with the same `repo`
        // identifier must be refused at load with the offending entry named.
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "owner/core"

[[repos]]
repo = "owner/core"
"#;
        let err = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect_err("duplicate repo entries must be refused");
        let field = err.field().expect("error must name the offending field");
        assert!(
            field.contains("repos[1].repo"),
            "error must name the duplicate entry's index, got `{field}`",
        );
        assert!(
            err.to_string().contains("owner/core"),
            "rendered error must echo the duplicate ghq identifier, got: {err}",
        );
    }

    #[test]
    fn legacy_workflow_path_under_repo_is_refused() {
        // Post-7.1a: per-repo `workflow_path` is gone. Existing configs that
        // still carry it must fail loudly (deny_unknown_fields surfaces this).
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "owner/core"
workflow_path = "/srv/git/core/WORKFLOW.md"
"#;
        let err = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect_err("legacy per-repo workflow_path must be refused");
        match err {
            ConfigError::Parse { ref field, .. } => {
                assert!(
                    field.contains("workflow_path"),
                    "expected error to name `workflow_path`, got `{field}`",
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn legacy_repo_scope_block_is_refused() {
        // Post-7.1a: `[[repos]].scope` is gone (agent-driven repo selection).
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "owner/core"

[repos.scope]
kind = "team"
key = "ENG"
"#;
        let err = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect_err("legacy `[repos.scope]` must be refused");
        match err {
            ConfigError::Parse { ref field, .. } => {
                assert!(
                    field.contains("scope"),
                    "expected error to name `scope`, got `{field}`",
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn linear_endpoint_override_round_trips() {
        // The 5.1 test-injection seam survives 7.1a: `[linear].endpoint`
        // remains a test-only override.
        let body = r#"
[linear]
token_env = "MY_LINEAR_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"
endpoint = "http://127.0.0.1:9999/graphql"

[workflow]
path = "/srv/policy/WORKFLOW.md"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
repo = "owner/core"
"#;
        let cfg = Config::load_from_str(body, &fixture_path(), &env_with_token())
            .expect("endpoint override must load");
        assert_eq!(
            cfg.linear_endpoint.as_deref(),
            Some("http://127.0.0.1:9999/graphql"),
        );
    }
}
