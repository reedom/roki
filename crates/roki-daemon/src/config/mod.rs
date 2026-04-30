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

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use repos::{LinearScope, RepoConfig};

/// Default polling cadence cap (Requirement 3.2: <= 5 min per scope).
pub const DEFAULT_POLLING_CADENCE_SECONDS: u64 = 300;

/// Hard upper bound on the polling cadence (5 minutes per Requirement 3.2).
pub const MAX_POLLING_CADENCE_SECONDS: u64 = 300;

/// Default for the operator-configurable max-concurrent-workers knob
/// (design.md "Performance & Scalability": "defaulted to a small integer").
pub const DEFAULT_MAX_CONCURRENT_WORKERS: u32 = 4;

/// Default environment variable name for the Linear API token.
pub const DEFAULT_LINEAR_TOKEN_ENV: &str = "LINEAR_API_TOKEN";

/// Root configuration for the roki daemon.
///
/// Field-level documentation lists the requirement each field traces to so
/// validation errors stay anchored to the spec.
#[derive(Debug, Clone)]
pub struct Config {
    /// Workspace root under which per-`(repo, issue)` directories are
    /// created. Requirement 4.1, 10.1.
    pub workspace_root: PathBuf,

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
    workspace_root: Option<PathBuf>,

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

    /// Override the workspace root.
    pub workspace_root: Option<PathBuf>,

    /// Override the polling cadence (seconds).
    pub polling_cadence_seconds: Option<u64>,

    /// Override the max-concurrent-workers knob.
    pub max_concurrent_workers: Option<u32>,
}

impl EnvOverrides {
    /// Read overrides from the process environment. Variables:
    ///
    /// * `ROKI_LINEAR_TOKEN` — literal token (highest precedence).
    /// * `ROKI_WORKSPACE_ROOT` — workspace root path.
    /// * `ROKI_POLLING_CADENCE_SECONDS` — polling cadence override.
    /// * `ROKI_MAX_CONCURRENT_WORKERS` — concurrency override.
    pub fn from_process_env() -> Result<Self, ConfigError> {
        let linear_token = env::var("ROKI_LINEAR_TOKEN").ok();
        let workspace_root = env::var("ROKI_WORKSPACE_ROOT").ok().map(PathBuf::from);
        let polling_cadence_seconds = parse_env_u64("ROKI_POLLING_CADENCE_SECONDS")?;
        let max_concurrent_workers = parse_env_u32("ROKI_MAX_CONCURRENT_WORKERS")?;
        Ok(Self {
            linear_token,
            workspace_root,
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
        let workspace_root = env.workspace_root.clone().or(file.workspace_root).ok_or(
            ConfigError::MissingField {
                field: "workspace_root".to_string(),
            },
        )?;

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

        Ok(Self {
            workspace_root,
            linear_token,
            polling_cadence: Duration::from_secs(polling_cadence_seconds),
            max_concurrent_workers,
            permission_strategy,
            repos: file.repos,
        })
    }
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

fn validate_repos(repos: &[RepoConfig]) -> Result<(), ConfigError> {
    for (index, repo) in repos.iter().enumerate() {
        if repo.id.trim().is_empty() {
            return Err(ConfigError::InvalidField {
                field: format!("repos[{index}].id"),
                reason: "must not be empty".to_string(),
            });
        }
        if repo.path.as_os_str().is_empty() {
            return Err(ConfigError::InvalidField {
                field: format!("repos[{index}].path"),
                reason: "must not be empty".to_string(),
            });
        }
        if repo.workflow_path.as_os_str().is_empty() {
            return Err(ConfigError::InvalidField {
                field: format!("repos[{index}].workflow_path"),
                reason: "must not be empty".to_string(),
            });
        }
        match &repo.scope {
            LinearScope::Team { key } if key.trim().is_empty() => {
                return Err(ConfigError::InvalidField {
                    field: format!("repos[{index}].scope.key"),
                    reason: "team key must not be empty".to_string(),
                });
            }
            LinearScope::Labels { any_of } if any_of.is_empty() => {
                return Err(ConfigError::InvalidField {
                    field: format!("repos[{index}].scope.any_of"),
                    reason: "label list must not be empty".to_string(),
                });
            }
            _ => {}
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
        r#"
workspace_root = "/var/lib/roki/workspaces"
polling_cadence_seconds = 120
max_concurrent_workers = 3

[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "allowlist"
settings = "/etc/roki/claude-settings.json"

[[repos]]
id = "core"
path = "/srv/git/core"
workflow_path = "/srv/git/core/WORKFLOW.md"

[repos.scope]
kind = "team"
key = "ENG"
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

        assert_eq!(
            cfg.workspace_root,
            PathBuf::from("/var/lib/roki/workspaces")
        );
        assert_eq!(cfg.polling_cadence, Duration::from_secs(120));
        assert_eq!(cfg.max_concurrent_workers, 3);
        assert_eq!(cfg.linear_token.expose(), "lin_api_test_secret");
        assert!(matches!(
            cfg.permission_strategy,
            PermissionStrategy::Allowlist { .. }
        ));
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.repos[0].id, "core");
        assert_eq!(
            cfg.repos[0].scope,
            LinearScope::Team {
                key: "ENG".to_string()
            }
        );
    }

    #[test]
    fn malformed_config_returns_error_naming_failing_field() {
        // `polling_cadence_seconds` is typed as a u64; a string here forces a
        // typed parse error whose offending field must be surfaced verbatim.
        let malformed = r#"
workspace_root = "/var/lib/roki/workspaces"
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
workspace_root = "/var/lib/roki/workspaces"

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
workspace_root = "/var/lib/roki/workspaces"

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
workspace_root = "/var/lib/roki/workspaces"

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
workspace_root = "/var/lib/roki/workspaces"
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
workspace_root = "/var/lib/roki/workspaces"
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
workspace_root = "/var/lib/roki/workspaces"

[linear]
token_file = "{}"

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
    fn invalid_repo_entry_names_the_offending_field() {
        let bad_repo = r#"
workspace_root = "/var/lib/roki/workspaces"

[linear]
token_env = "MY_LINEAR_TOKEN"

[permissions]
strategy = "dangerously_skip_permissions"

[[repos]]
id = ""
path = "/srv/git/core"
workflow_path = "/srv/git/core/WORKFLOW.md"

[repos.scope]
kind = "team"
key = "ENG"
"#;
        let err = Config::load_from_str(bad_repo, &fixture_path(), &env_with_token())
            .expect_err("empty repo id must be refused");
        assert_eq!(err.field(), Some("repos[0].id"));
    }
}
