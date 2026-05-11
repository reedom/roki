// Walking-skeleton tasks land in dependency order: this loader (task 2.1)
// precedes the runtime wiring (later task in the `runtime` boundary) that
// will call `RokiConfig::load`. Until that wiring lands, the loader and
// its private helpers are exercised only by the unit tests below, which
// triggers `dead_code` for the leaf API. Allow it module-locally instead
// of leaking the relaxation crate-wide.
#![allow(dead_code)]

//! `roki.toml` loader.
//!
//! Reads the canonical sections (`[linear]`, `[linear.webhook]`,
//! `[default.ai]`, `[engine]`, `[paths]`, `[log]`, `[escalation]`) per
//! [`ref:config`](../../../docs/reference/config.md). Slice 8 dropped
//! `[default.ai.session]` (no more long-lived AI session shape) and
//! collapsed `[default.ai.command]` into `[default.ai]`.
//!
//! `[linear].token` is held in process memory; the hand-rolled `Debug`
//! impl on `LinearSection` masks it as `***` so tracing emissions of the
//! whole `RokiConfig` cannot leak the token (design "Cross-Cutting
//! Concerns").

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::RokiConfigError;

/// Top-level loaded configuration.
///
/// `Debug` is hand-rolled so that `[linear].token` is masked when the
/// whole struct is formatted (e.g., `tracing::debug!(?cfg)`).
pub struct RokiConfig {
    pub linear: LinearSection,
    pub linear_webhook: LinearWebhookSection,
    pub default_ai: DefaultAiSection,
    pub engine: EngineSection,
    pub paths: PathsSection,
    pub log: LogSection,
    pub escalation: EscalationSection,
    pub api: ApiSection,
}

/// `[linear]` section. Only `token` is required at the skeleton level.
///
/// Custom `Debug` masks the token field.
#[derive(Clone)]
pub struct LinearSection {
    pub token: String,
    pub polling: LinearPollingSection,
}

impl fmt::Debug for LinearSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinearSection")
            .field("token", &"***")
            .field("polling", &self.polling)
            .finish()
    }
}

/// `[linear.polling]` section. Cadence for the polling fallback path.
#[derive(Clone, Debug, PartialEq)]
pub struct LinearPollingSection {
    pub cadence_seconds: u32,
}

impl Default for LinearPollingSection {
    fn default() -> Self {
        Self {
            cadence_seconds: 300,
        }
    }
}

/// `[api]` section. Observability HTTP API server.
///
/// `port` absent disables the API server (server gating).
#[derive(Clone, Debug, PartialEq)]
pub struct ApiSection {
    pub bind: String,
    pub port: Option<u16>,
    pub ticket_events_window: u32,
    pub cycle_list_window: u32,
}

impl Default for ApiSection {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: None,
            ticket_events_window: 50,
            cycle_list_window: 50,
        }
    }
}

/// `[linear.webhook]` section.
#[derive(Clone, Debug)]
pub struct LinearWebhookSection {
    pub bind: String,
    pub port: u16,
    /// Accepted-without-applying per Req 2.4. The skeleton does not
    /// verify HMAC even when set (Req 3.3).
    pub secret: Option<String>,
}

/// `[default.ai]` section. Slice 8 collapsed `[default.ai.command]` into
/// this section and dropped `[default.ai.session]` entirely.
#[derive(Clone, Debug)]
pub struct DefaultAiSection {
    pub cli: String,
    /// Stdout-silence threshold in seconds; defaults to `300`.
    pub stall_seconds: u32,
}

/// `[engine]` section.
///
/// `max_iterations` is enforced at load time: absent → default 10,
/// present but zero → `TypeMismatch` error.
/// `shutdown_window_seconds` is enforced at load time: absent → default 30,
/// present but outside 1..=600 → `TypeMismatch` error.
#[derive(Clone, Debug)]
pub struct EngineSection {
    pub max_iterations: u32,
    pub shutdown_window_seconds: u32,
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            max_iterations: 10,
            shutdown_window_seconds: 30,
        }
    }
}

/// `[paths]` section.
#[derive(Clone, Debug)]
pub struct PathsSection {
    pub workflow: PathBuf,
    pub session_root: PathBuf,
}

/// `[log]` section. No fields are required at the skeleton level;
/// canonical keys (`destination`, `level`, `file_path`, `ring_size`) are
/// accepted-without-applying.
#[derive(Clone, Debug, Default)]
pub struct LogSection {
    pub destination: Option<String>,
    pub level: Option<String>,
    pub file_path: Option<PathBuf>,
    pub ring_size: Option<u32>,
}

/// `[escalation]` section. Bounds the in-memory escalation queue
/// (fr:06 §Escalation queue).
#[derive(Clone, Debug)]
pub struct EscalationSection {
    pub queue_size: u32,
}

impl Default for EscalationSection {
    fn default() -> Self {
        Self { queue_size: 64 }
    }
}

impl fmt::Debug for RokiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RokiConfig")
            .field("linear", &self.linear)
            .field("linear_webhook", &self.linear_webhook)
            .field("default_ai", &self.default_ai)
            .field("engine", &self.engine)
            .field("paths", &self.paths)
            .field("log", &self.log)
            .field("escalation", &self.escalation)
            .field("api", &self.api)
            .finish()
    }
}

impl RokiConfig {
    /// Load and validate `roki.toml` from `path`.
    ///
    /// Returns `RokiConfigError::MissingFile` when the file is absent,
    /// `Unreadable` for I/O errors, `Parse` for TOML syntax errors,
    /// and `MissingField { key: <dotted.path> }` when any required
    /// field is absent or has the wrong type.
    pub fn load(path: &Path) -> Result<Self, RokiConfigError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(RokiConfigError::MissingFile {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(RokiConfigError::Unreadable {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        // Permissive deserialization: parse into a shape with all required
        // fields as `Option`, then validate each one with a key-path-bearing
        // error per design `config::roki` invariant. Unknown keys are
        // tolerated because we omit `deny_unknown_fields` everywhere.
        let raw_cfg: RawRokiConfig =
            toml::from_str(&raw).map_err(|source| RokiConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;

        raw_cfg.validate(path)
    }

    /// Build the smallest legal `RokiConfig` for unit tests.
    ///
    /// Mirrors the canonical TOML shape used by slice 1-4 e2e fixtures:
    /// every required section populated with placeholder values rooted at
    /// `session_root`. The returned config is suitable for tests that
    /// thread a config through code paths but do not actually consume any
    /// of its fields beyond construction.
    ///
    /// `#[cfg(test)]` so production code cannot reach the helper.
    #[cfg(test)]
    pub fn test_default(session_root: &Path) -> Self {
        Self {
            linear: LinearSection {
                token: "x".to_string(),
                polling: LinearPollingSection::default(),
            },
            linear_webhook: LinearWebhookSection {
                bind: "127.0.0.1".to_string(),
                port: 0,
                secret: None,
            },
            default_ai: DefaultAiSection {
                cli: "echo".to_string(),
                stall_seconds: 300,
            },
            engine: EngineSection::default(),
            paths: PathsSection {
                workflow: session_root.join("WORKFLOW.yaml"),
                session_root: session_root.to_path_buf(),
            },
            log: LogSection::default(),
            escalation: EscalationSection::default(),
            api: ApiSection::default(),
        }
    }
}

// ---------- Permissive raw shape for staged validation ----------

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawRokiConfig {
    linear: Option<RawLinear>,
    #[serde(rename = "default")]
    default_block: Option<RawDefaultBlock>,
    engine: Option<RawEngine>,
    paths: Option<RawPaths>,
    log: Option<RawLog>,
    escalation: Option<RawEscalation>,
    api: Option<RawApi>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawLinear {
    token: Option<String>,
    webhook: Option<RawLinearWebhook>,
    polling: Option<RawLinearPolling>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawLinearPolling {
    cadence_seconds: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawApi {
    bind: Option<String>,
    port: Option<u16>,
    ticket_events_window: Option<u32>,
    cycle_list_window: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawLinearWebhook {
    bind: Option<String>,
    port: Option<u16>,
    secret: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawDefaultBlock {
    ai: Option<RawDefaultAi>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawDefaultAi {
    cli: Option<String>,
    stall_seconds: Option<i64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEngine {
    max_iterations: Option<u32>,
    shutdown_window_seconds: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawPaths {
    workflow: Option<PathBuf>,
    session_root: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawLog {
    destination: Option<String>,
    level: Option<String>,
    file_path: Option<PathBuf>,
    ring_size: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEscalation {
    queue_size: Option<u32>,
}

impl RawRokiConfig {
    fn validate(self, path: &Path) -> Result<RokiConfig, RokiConfigError> {
        let raw_linear = self.linear.unwrap_or_default();
        let raw_webhook = raw_linear.webhook.unwrap_or_default();
        let raw_default = self.default_block.unwrap_or_default();
        let raw_default_ai = raw_default.ai.unwrap_or_default();
        let raw_engine = self.engine.unwrap_or_default();
        let raw_paths = self.paths.unwrap_or_default();
        let raw_log = self.log.unwrap_or_default();

        let polling = parse_linear_polling(path, raw_linear.polling.unwrap_or_default())?;
        let linear = LinearSection {
            token: required_string(path, "linear.token", raw_linear.token)?,
            polling,
        };

        let linear_webhook = LinearWebhookSection {
            bind: required_string(path, "linear.webhook.bind", raw_webhook.bind)?,
            port: required_field(path, "linear.webhook.port", raw_webhook.port)?,
            secret: raw_webhook.secret,
        };

        let stall = parse_stall_seconds(
            path,
            "default.ai.stall_seconds",
            raw_default_ai.stall_seconds,
            300,
        )?;
        let default_ai = DefaultAiSection {
            cli: required_string(path, "default.ai.cli", raw_default_ai.cli)?,
            stall_seconds: stall,
        };

        let paths_section = PathsSection {
            workflow: required_field(path, "paths.workflow", raw_paths.workflow)?,
            session_root: required_field(path, "paths.session_root", raw_paths.session_root)?,
        };

        let engine = parse_engine(path, raw_engine)?;

        let log = LogSection {
            destination: raw_log.destination,
            level: raw_log.level,
            file_path: raw_log.file_path,
            ring_size: raw_log.ring_size,
        };

        let raw_escalation = self.escalation.unwrap_or_default();
        let escalation = parse_escalation(path, raw_escalation)?;

        let api = parse_api(path, self.api.unwrap_or_default())?;

        Ok(RokiConfig {
            linear,
            linear_webhook,
            default_ai,
            engine,
            paths: paths_section,
            log,
            escalation,
            api,
        })
    }
}

fn parse_stall_seconds(
    path: &Path,
    key: &'static str,
    raw: Option<i64>,
    default: u32,
) -> Result<u32, RokiConfigError> {
    match raw {
        None => Ok(default),
        Some(n) if n >= 1 => Ok(n as u32),
        Some(_) => Err(RokiConfigError::TypeMismatch {
            path: path.to_path_buf(),
            key: key.to_string(),
            expected: "integer >= 1",
        }),
    }
}

fn parse_engine(path: &Path, raw: RawEngine) -> Result<EngineSection, RokiConfigError> {
    let max_iterations = match raw.max_iterations {
        None => 10,
        Some(0) => {
            return Err(RokiConfigError::TypeMismatch {
                path: path.to_path_buf(),
                key: "engine.max_iterations".to_string(),
                expected: "u32 >= 1",
            });
        }
        Some(n) => n,
    };
    let shutdown_window_seconds = match raw.shutdown_window_seconds {
        None => 30,
        Some(n) if (1..=600).contains(&n) => n,
        Some(_) => {
            return Err(RokiConfigError::TypeMismatch {
                path: path.to_path_buf(),
                key: "engine.shutdown_window_seconds".to_string(),
                expected: "u32 in 1..=600",
            });
        }
    };
    Ok(EngineSection {
        max_iterations,
        shutdown_window_seconds,
    })
}

fn parse_linear_polling(
    path: &Path,
    raw: RawLinearPolling,
) -> Result<LinearPollingSection, RokiConfigError> {
    // Codes are baked into `expected:` so error messages reproducibly
    // contain a stable token (e.g., `invalid_cadence`) that callers can
    // pattern-match without parsing free-form text.
    let cadence_seconds = match raw.cadence_seconds {
        None => 300,
        Some(n) if n >= 60 => n,
        Some(_) => {
            return Err(RokiConfigError::TypeMismatch {
                path: path.to_path_buf(),
                key: "linear.polling.cadence_seconds".to_string(),
                expected: "u32 >= 60 (invalid_cadence)",
            });
        }
    };
    Ok(LinearPollingSection { cadence_seconds })
}

fn parse_api(path: &Path, raw: RawApi) -> Result<ApiSection, RokiConfigError> {
    let bind = raw.bind.unwrap_or_else(|| "127.0.0.1".to_string());
    if bind.parse::<std::net::IpAddr>().is_err() {
        return Err(RokiConfigError::TypeMismatch {
            path: path.to_path_buf(),
            key: "api.bind".to_string(),
            expected: "IP address (invalid_bind_addr)",
        });
    }
    if let Some(p) = raw.port {
        if p == 0 {
            return Err(RokiConfigError::TypeMismatch {
                path: path.to_path_buf(),
                key: "api.port".to_string(),
                expected: "u16 >= 1 (invalid_port_zero)",
            });
        }
    }
    let ticket_events_window = raw.ticket_events_window.unwrap_or(50);
    if !(1..=500).contains(&ticket_events_window) {
        return Err(RokiConfigError::TypeMismatch {
            path: path.to_path_buf(),
            key: "api.ticket_events_window".to_string(),
            expected: "u32 in 1..=500 (invalid_window)",
        });
    }
    let cycle_list_window = raw.cycle_list_window.unwrap_or(50);
    if !(1..=500).contains(&cycle_list_window) {
        return Err(RokiConfigError::TypeMismatch {
            path: path.to_path_buf(),
            key: "api.cycle_list_window".to_string(),
            expected: "u32 in 1..=500 (invalid_window)",
        });
    }
    Ok(ApiSection {
        bind,
        port: raw.port,
        ticket_events_window,
        cycle_list_window,
    })
}

fn parse_escalation(path: &Path, raw: RawEscalation) -> Result<EscalationSection, RokiConfigError> {
    let queue_size = match raw.queue_size {
        None => 64,
        Some(n) if (1..=1024).contains(&n) => n,
        Some(_) => {
            return Err(RokiConfigError::TypeMismatch {
                path: path.to_path_buf(),
                key: "escalation.queue_size".to_string(),
                expected: "u32 in 1..=1024",
            });
        }
    };
    Ok(EscalationSection { queue_size })
}

fn required_field<T>(
    path: &Path,
    key: &'static str,
    value: Option<T>,
) -> Result<T, RokiConfigError> {
    value.ok_or_else(|| RokiConfigError::MissingField {
        path: path.to_path_buf(),
        key: key.to_string(),
    })
}

fn required_string(
    path: &Path,
    key: &'static str,
    value: Option<String>,
) -> Result<String, RokiConfigError> {
    // Treat empty strings as "missing" because TOML cannot omit a typed
    // string field once a stricter validator wraps this: the skeleton's
    // canonical reference says `[linear].token` "Refuses startup if
    // missing", which an empty token would equally violate.
    match value {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(RokiConfigError::MissingField {
            path: path.to_path_buf(),
            key: key.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_toml(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("roki.toml");
        let mut f = std::fs::File::create(&path).expect("create toml");
        f.write_all(body.as_bytes()).expect("write toml");
        path
    }

    fn parse_test(toml: &str) -> Result<RokiConfig, RokiConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("roki.toml");
        std::fs::write(&p, toml).unwrap();
        RokiConfig::load(&p)
    }

    #[test]
    fn api_section_defaults_when_block_absent() {
        let toml = r#"
[linear]
token = "x"
[linear.webhook]
bind = "127.0.0.1"
port = 1
[default.ai]
cli = "echo"
[engine]
[paths]
workflow = "WORKFLOW.yaml"
session_root = "/tmp"
[log]
"#;
        let cfg: RokiConfig = parse_test(toml).unwrap();
        assert_eq!(cfg.api.bind, "127.0.0.1");
        assert!(cfg.api.port.is_none());
        assert_eq!(cfg.api.ticket_events_window, 50);
        assert_eq!(cfg.api.cycle_list_window, 50);
        assert_eq!(cfg.linear.polling.cadence_seconds, 300);
    }

    #[test]
    fn api_section_validates_port_zero() {
        let toml = r#"
[linear]
token = "x"
[linear.webhook]
bind = "127.0.0.1"
port = 1
[default.ai]
cli = "echo"
[engine]
[paths]
workflow = "WORKFLOW.yaml"
session_root = "/tmp"
[log]
[api]
port = 0
"#;
        let err = parse_test(toml).unwrap_err();
        assert!(err.to_string().contains("invalid_port_zero"));
    }

    #[test]
    fn polling_cadence_min_60() {
        let toml = r#"
[linear]
token = "x"
[linear.polling]
cadence_seconds = 30
[linear.webhook]
bind = "127.0.0.1"
port = 1
[default.ai]
cli = "echo"
[engine]
[paths]
workflow = "WORKFLOW.yaml"
session_root = "/tmp"
[log]
"#;
        let err = parse_test(toml).unwrap_err();
        assert!(err.to_string().contains("invalid_cadence"));
    }

    #[test]
    fn test_default_yields_legal_engine_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = RokiConfig::test_default(dir.path());
        assert!(cfg.engine.max_iterations >= 1);
        assert_eq!(cfg.engine.shutdown_window_seconds, 30);
        assert_eq!(cfg.paths.session_root, dir.path());
        assert_eq!(cfg.linear_webhook.bind, "127.0.0.1");
    }

    const HAPPY_PATH_TOML: &str = r#"
[linear]
token = "lin_api_secret_value"

[linear.webhook]
bind = "127.0.0.1"
port = 8080
secret = "wh_secret"

[default.ai]
cli = "claude --print"

[engine]
max_iterations = 5

[paths]
workflow = "/etc/roki/WORKFLOW.yaml"
session_root = "/var/roki/sessions"

[log]
level = "info"
destination = "stdout"
"#;

    #[test]
    fn happy_path_loads_all_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, HAPPY_PATH_TOML);

        let cfg = RokiConfig::load(&path).expect("happy path should load");

        assert_eq!(cfg.linear.token, "lin_api_secret_value");
        assert_eq!(cfg.linear_webhook.bind, "127.0.0.1");
        assert_eq!(cfg.linear_webhook.port, 8080);
        assert_eq!(cfg.linear_webhook.secret.as_deref(), Some("wh_secret"));
        assert_eq!(cfg.default_ai.cli, "claude --print");
        assert_eq!(
            cfg.paths.workflow,
            std::path::PathBuf::from("/etc/roki/WORKFLOW.yaml")
        );
        assert_eq!(
            cfg.paths.session_root,
            std::path::PathBuf::from("/var/roki/sessions")
        );
    }

    #[test]
    fn missing_linear_token_returns_key_path_error() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[linear]
# token missing on purpose

[linear.webhook]
bind = "127.0.0.1"
port = 8080

[default.ai]
cli = "claude --print"

[paths]
workflow = "/etc/roki/WORKFLOW.yaml"
session_root = "/var/roki/sessions"

[engine]

[log]
"#;
        let path = write_toml(&dir, body);

        let err = RokiConfig::load(&path).expect_err("missing token must fail");
        match err {
            crate::error::RokiConfigError::MissingField { key, .. } => {
                assert_eq!(key, "linear.token");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn missing_webhook_bind_returns_key_path_error() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[linear]
token = "tok"

[linear.webhook]
port = 8080

[default.ai]
cli = "claude --print"

[paths]
workflow = "/etc/roki/WORKFLOW.yaml"
session_root = "/var/roki/sessions"

[engine]

[log]
"#;
        let path = write_toml(&dir, body);

        let err = RokiConfig::load(&path).expect_err("missing bind must fail");
        match err {
            crate::error::RokiConfigError::MissingField { key, .. } => {
                assert_eq!(key, "linear.webhook.bind");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn unknown_top_level_key_is_silently_retained() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("{HAPPY_PATH_TOML}\n[unknown]\nfoo = \"bar\"\nbaz = 42\n");
        let path = write_toml(&dir, &body);

        let cfg = RokiConfig::load(&path).expect("unknown sections must not fail loading");
        // Required fields still populated.
        assert_eq!(cfg.linear.token, "lin_api_secret_value");
    }

    #[test]
    fn webhook_secret_loads_without_applying() {
        let dir = tempfile::tempdir().unwrap();
        // Same as happy path; just assert the secret is present.
        let path = write_toml(&dir, HAPPY_PATH_TOML);
        let cfg = RokiConfig::load(&path).expect("loads ok");
        assert_eq!(cfg.linear_webhook.secret.as_deref(), Some("wh_secret"));
    }

    #[test]
    fn debug_impl_masks_linear_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, HAPPY_PATH_TOML);
        let cfg = RokiConfig::load(&path).expect("loads ok");

        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("lin_api_secret_value"),
            "Debug output must not contain raw token: {dbg}"
        );
        assert!(
            dbg.contains("***"),
            "Debug output must mask token with ***: {dbg}"
        );

        // Also assert the masked form on the section itself, since
        // RokiConfig's Debug delegates section-by-section.
        let section_dbg = format!("{:?}", cfg.linear);
        assert!(!section_dbg.contains("lin_api_secret_value"));
        assert!(section_dbg.contains("***"));
    }

    #[test]
    fn missing_file_returns_missing_file_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let err = RokiConfig::load(&missing).expect_err("missing file fails");
        match err {
            crate::error::RokiConfigError::MissingFile { path } => {
                assert_eq!(path, missing);
            }
            other => panic!("expected MissingFile, got {other:?}"),
        }
    }

    #[test]
    fn max_iterations_defaults_to_ten_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let toml_body = r#"
[linear]
token = "x"

[linear.webhook]
bind = "127.0.0.1"
port = 8000

[default.ai]
cli = "echo"

[engine]

[paths]
workflow = "/tmp/w.toml"
session_root = "/tmp/sess"

[log]
"#;
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml_body).unwrap();

        let cfg = RokiConfig::load(&path).expect("load");
        assert_eq!(cfg.engine.max_iterations, 10);
    }

    #[test]
    fn stall_seconds_defaults_when_absent() {
        let toml = r#"
[linear]
token = "t"

[linear.webhook]
bind = "127.0.0.1"
port = 7000

[default.ai]
cli = "claude --print"

[engine]

[paths]
workflow = "./WORKFLOW.yaml"
session_root = "./.roki/sessions"
"#;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = RokiConfig::load(&path).unwrap();
        assert_eq!(cfg.default_ai.stall_seconds, 300);
    }

    #[test]
    fn stall_seconds_zero_is_rejected() {
        let toml = r#"
[linear]
token = "t"

[linear.webhook]
bind = "127.0.0.1"
port = 7000

[default.ai]
cli = "claude --print"
stall_seconds = 0

[engine]

[paths]
workflow = "./WORKFLOW.yaml"
session_root = "./.roki/sessions"
"#;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml).unwrap();
        let err = RokiConfig::load(&path).unwrap_err();
        match err {
            RokiConfigError::TypeMismatch { key, .. } => {
                assert_eq!(key, "default.ai.stall_seconds");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn max_iterations_zero_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let toml_body = r#"
[linear]
token = "x"

[linear.webhook]
bind = "127.0.0.1"
port = 8000

[default.ai]
cli = "echo"

[engine]
max_iterations = 0

[paths]
workflow = "/tmp/w.toml"
session_root = "/tmp/sess"

[log]
"#;
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml_body).unwrap();

        let err = RokiConfig::load(&path).expect_err("rejects zero");
        let msg = format!("{err}");
        assert!(msg.contains("max_iterations"), "msg: {msg}");
    }

    #[test]
    fn shutdown_window_seconds_defaults_to_30() {
        let dir = tempfile::tempdir().unwrap();
        let toml_body = r#"
[linear]
token = "x"

[linear.webhook]
bind = "127.0.0.1"
port = 8000

[default.ai]
cli = "echo"

[engine]

[paths]
workflow = "/tmp/w.toml"
session_root = "/tmp/sess"

[log]
"#;
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml_body).unwrap();

        let cfg = RokiConfig::load(&path).expect("load");
        assert_eq!(cfg.engine.shutdown_window_seconds, 30);
    }

    #[test]
    fn shutdown_window_seconds_below_min_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let toml_body = r#"
[linear]
token = "x"

[linear.webhook]
bind = "127.0.0.1"
port = 8000

[default.ai]
cli = "echo"

[engine]
shutdown_window_seconds = 0

[paths]
workflow = "/tmp/w.toml"
session_root = "/tmp/sess"

[log]
"#;
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml_body).unwrap();

        let err = RokiConfig::load(&path).expect_err("rejects 0");
        let msg = format!("{err}");
        assert!(msg.contains("shutdown_window_seconds"), "msg: {msg}");
    }

    #[test]
    fn shutdown_window_seconds_above_max_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let toml_body = r#"
[linear]
token = "x"

[linear.webhook]
bind = "127.0.0.1"
port = 8000

[default.ai]
cli = "echo"

[engine]
shutdown_window_seconds = 601

[paths]
workflow = "/tmp/w.toml"
session_root = "/tmp/sess"

[log]
"#;
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml_body).unwrap();

        let err = RokiConfig::load(&path).expect_err("rejects 601");
        let msg = format!("{err}");
        assert!(msg.contains("shutdown_window_seconds"), "msg: {msg}");
    }

    #[test]
    fn escalation_default_is_64() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, HAPPY_PATH_TOML);
        let cfg = RokiConfig::load(&path).expect("happy path");
        assert_eq!(cfg.escalation.queue_size, 64);
    }

    #[test]
    fn escalation_explicit_value_is_honored() {
        let body = format!("{}\n[escalation]\nqueue_size = 256\n", HAPPY_PATH_TOML);
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, &body);
        let cfg = RokiConfig::load(&path).expect("explicit ok");
        assert_eq!(cfg.escalation.queue_size, 256);
    }

    #[test]
    fn escalation_zero_is_rejected() {
        let body = format!("{}\n[escalation]\nqueue_size = 0\n", HAPPY_PATH_TOML);
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, &body);
        let err = RokiConfig::load(&path).expect_err("zero rejected");
        match err {
            crate::error::RokiConfigError::TypeMismatch { key, .. } => {
                assert_eq!(key, "escalation.queue_size");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn escalation_above_max_is_rejected() {
        let body = format!("{}\n[escalation]\nqueue_size = 2000\n", HAPPY_PATH_TOML);
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, &body);
        let err = RokiConfig::load(&path).expect_err("above max rejected");
        match err {
            crate::error::RokiConfigError::TypeMismatch { key, .. } => {
                assert_eq!(key, "escalation.queue_size");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }
}
