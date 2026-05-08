// Walking-skeleton tasks land in dependency order: this loader (task 2.1)
// precedes the runtime wiring (later task in the `runtime` boundary) that
// will call `RokiConfig::load`. Until that wiring lands, the loader and
// its private helpers are exercised only by the unit tests below, which
// triggers `dead_code` for the leaf API. Allow it module-locally instead
// of leaking the relaxation crate-wide.
#![allow(dead_code)]

//! `roki.toml` loader for the walking-skeleton daemon.
//!
//! Reads the six canonical sections the skeleton path needs (`[linear]`,
//! `[linear.webhook]`, `[default.ai.command]`, `[engine]`, `[paths]`,
//! `[log]`) per [`ref:config`](../../../docs/reference/config.md).
//! Required-field set per design `config::roki`. Unknown keys and
//! accepted-without-applying keys (`[default.ai.session]`,
//! `[linear.webhook].secret`) load silently.
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
    pub default_ai_command: DefaultAiCommandSection,
    pub engine: EngineSection,
    pub paths: PathsSection,
    pub log: LogSection,
    /// `[default.ai.session]` section. Slice 2 loads it eagerly so cycles
    /// can resolve the session subprocess cli + stall window.
    pub default_ai_session: Option<DefaultAiSessionSection>,
}

/// `[linear]` section. Only `token` is required at the skeleton level.
///
/// Custom `Debug` masks the token field.
#[derive(Clone)]
pub struct LinearSection {
    pub token: String,
}

impl fmt::Debug for LinearSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinearSection")
            .field("token", &"***")
            .finish()
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

/// `[default.ai.command]` section.
#[derive(Clone, Debug)]
pub struct DefaultAiCommandSection {
    pub cli: String,
    /// Stdout-silence threshold in seconds; defaults to 300. Per shape default in
    /// docs/reference/config.md.
    pub stall_seconds: u32,
}

/// `[default.ai.session]` section.
///
/// Slice 2 promotes the section from "accepted-without-applying" to a
/// loaded shape: `cli` stays optional (only required when an actual phase
/// resolves to session shape, checked at cycle start), `stall_seconds` gets
/// the canonical default 600.
#[derive(Clone, Debug)]
pub struct DefaultAiSessionSection {
    pub cli: Option<String>,
    /// Stdout-silence threshold in seconds; defaults to 600. Applied also to the
    /// iter_exhausted post-stdin-close grace per fr:01 §123-125.
    pub stall_seconds: u32,
}

/// `[engine]` section.
///
/// `max_iterations` is enforced at load time: absent → default 10,
/// present but zero → `TypeMismatch` error.
#[derive(Clone, Debug)]
pub struct EngineSection {
    pub max_iterations: u32,
}

impl Default for EngineSection {
    fn default() -> Self {
        Self { max_iterations: 10 }
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

impl fmt::Debug for RokiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RokiConfig")
            .field("linear", &self.linear)
            .field("linear_webhook", &self.linear_webhook)
            .field("default_ai_command", &self.default_ai_command)
            .field("engine", &self.engine)
            .field("paths", &self.paths)
            .field("log", &self.log)
            .field("default_ai_session", &self.default_ai_session)
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
            },
            linear_webhook: LinearWebhookSection {
                bind: "127.0.0.1".to_string(),
                port: 0,
                secret: None,
            },
            default_ai_command: DefaultAiCommandSection {
                cli: "echo".to_string(),
                stall_seconds: 300,
            },
            engine: EngineSection::default(),
            paths: PathsSection {
                workflow: session_root.join("WORKFLOW.toml"),
                session_root: session_root.to_path_buf(),
            },
            log: LogSection::default(),
            default_ai_session: None,
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
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawLinear {
    token: Option<String>,
    webhook: Option<RawLinearWebhook>,
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
    command: Option<RawDefaultAiCommand>,
    session: Option<RawDefaultAiSession>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawDefaultAiCommand {
    cli: Option<String>,
    stall_seconds: Option<i64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawDefaultAiSession {
    cli: Option<String>,
    stall_seconds: Option<i64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEngine {
    max_iterations: Option<u32>,
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

impl RawRokiConfig {
    fn validate(self, path: &Path) -> Result<RokiConfig, RokiConfigError> {
        let raw_linear = self.linear.unwrap_or_default();
        let raw_webhook = raw_linear.webhook.unwrap_or_default();
        let raw_default = self.default_block.unwrap_or_default();
        let raw_default_ai = raw_default.ai.unwrap_or_default();
        let raw_default_command = raw_default_ai.command.unwrap_or_default();
        let raw_engine = self.engine.unwrap_or_default();
        let raw_paths = self.paths.unwrap_or_default();
        let raw_log = self.log.unwrap_or_default();

        let linear = LinearSection {
            token: required_string(path, "linear.token", raw_linear.token)?,
        };

        let linear_webhook = LinearWebhookSection {
            bind: required_string(path, "linear.webhook.bind", raw_webhook.bind)?,
            port: required_field(path, "linear.webhook.port", raw_webhook.port)?,
            secret: raw_webhook.secret,
        };

        let cmd_stall = parse_stall_seconds(
            path,
            "default.ai.command.stall_seconds",
            raw_default_command.stall_seconds,
            300,
        )?;
        let default_ai_command = DefaultAiCommandSection {
            cli: required_string(path, "default.ai.command.cli", raw_default_command.cli)?,
            stall_seconds: cmd_stall,
        };

        let default_ai_session = match raw_default_ai.session {
            None => None,
            Some(raw_session) => {
                let session_stall = parse_stall_seconds(
                    path,
                    "default.ai.session.stall_seconds",
                    raw_session.stall_seconds,
                    600,
                )?;
                Some(DefaultAiSessionSection {
                    cli: raw_session.cli,
                    stall_seconds: session_stall,
                })
            }
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

        Ok(RokiConfig {
            linear,
            linear_webhook,
            default_ai_command,
            engine,
            paths: paths_section,
            log,
            default_ai_session,
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
    Ok(EngineSection { max_iterations })
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

    #[test]
    fn test_default_yields_legal_engine_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = RokiConfig::test_default(dir.path());
        assert!(cfg.engine.max_iterations >= 1);
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

[default.ai.command]
cli = "claude --print"

[default.ai.session]
cli = "claude-code"

[engine]
max_iterations = 5

[paths]
workflow = "/etc/roki/WORKFLOW.toml"
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
        assert_eq!(cfg.default_ai_command.cli, "claude --print");
        assert_eq!(
            cfg.paths.workflow,
            std::path::PathBuf::from("/etc/roki/WORKFLOW.toml")
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

[default.ai.command]
cli = "claude --print"

[paths]
workflow = "/etc/roki/WORKFLOW.toml"
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

[default.ai.command]
cli = "claude --print"

[paths]
workflow = "/etc/roki/WORKFLOW.toml"
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
    fn accepted_without_applying_default_ai_session_loads_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, HAPPY_PATH_TOML);

        let cfg = RokiConfig::load(&path).expect("loads ok");
        let session = cfg
            .default_ai_session
            .as_ref()
            .expect("default.ai.session table present");
        assert_eq!(session.cli.as_deref(), Some("claude-code"));
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

[default.ai.command]
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

[default.ai.command]
cli = "claude --print"

[default.ai.session]
cli = "claude --interactive"

[engine]

[paths]
workflow = "./WORKFLOW.toml"
session_root = "./.roki/sessions"
"#;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = RokiConfig::load(&path).unwrap();
        assert_eq!(cfg.default_ai_command.stall_seconds, 300);
        let session = cfg.default_ai_session.as_ref().expect("session present");
        assert_eq!(session.stall_seconds, 600);
    }

    #[test]
    fn stall_seconds_zero_is_rejected() {
        let toml = r#"
[linear]
token = "t"

[linear.webhook]
bind = "127.0.0.1"
port = 7000

[default.ai.command]
cli = "claude --print"
stall_seconds = 0

[engine]

[paths]
workflow = "./WORKFLOW.toml"
session_root = "./.roki/sessions"
"#;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml).unwrap();
        let err = RokiConfig::load(&path).unwrap_err();
        match err {
            RokiConfigError::TypeMismatch { key, .. } => {
                assert_eq!(key, "default.ai.command.stall_seconds");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn stall_seconds_session_zero_is_rejected() {
        let toml = r#"
[linear]
token = "t"

[linear.webhook]
bind = "127.0.0.1"
port = 7000

[default.ai.command]
cli = "claude --print"

[default.ai.session]
cli = "claude --interactive"
stall_seconds = 0

[engine]

[paths]
workflow = "./WORKFLOW.toml"
session_root = "./.roki/sessions"
"#;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml).unwrap();
        let err = RokiConfig::load(&path).unwrap_err();
        match err {
            RokiConfigError::TypeMismatch { key, .. } => {
                assert_eq!(key, "default.ai.session.stall_seconds");
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

[default.ai.command]
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
}
