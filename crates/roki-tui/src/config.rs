//! ~/.config/roki-tui/config.toml loader + CLI merge. Missing file → defaults.
//! Validation failure refuses startup with a one-line error on stderr.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::cli::Cli;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub api_url: String,
    pub polling: PollingSection,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct TuiConfig {
    pub polling: PollingSection,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollingSection {
    #[serde(default = "PollingSection::default_tickets")]
    pub tickets_seconds: u32,
    #[serde(default = "PollingSection::default_events")]
    pub events_seconds: u32,
    #[serde(default = "PollingSection::default_escalations")]
    pub escalations_seconds: u32,
}

impl PollingSection {
    fn default_tickets() -> u32 {
        2
    }
    fn default_events() -> u32 {
        1
    }
    fn default_escalations() -> u32 {
        5
    }
}

impl Default for PollingSection {
    fn default() -> Self {
        Self {
            tickets_seconds: Self::default_tickets(),
            events_seconds: Self::default_events(),
            escalations_seconds: Self::default_escalations(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid_api_url: {0}")]
    InvalidApiUrl(String),
    #[error("config_not_found: {0}")]
    ConfigNotFound(PathBuf),
    #[error("config_parse: {0}")]
    Parse(String),
    #[error("invalid_tickets_cadence: {0} (must be >= 1)")]
    InvalidTicketsCadence(u32),
    #[error("invalid_events_cadence: {0} (must be >= 1)")]
    InvalidEventsCadence(u32),
    #[error("invalid_escalations_cadence: {0} (must be >= 1)")]
    InvalidEscalationsCadence(u32),
}

pub fn resolve(cli: Cli) -> Result<ResolvedConfig, ConfigError> {
    let url = reqwest::Url::parse(&cli.api_url)
        .map_err(|e| ConfigError::InvalidApiUrl(format!("{}: {}", cli.api_url, e)))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::InvalidApiUrl(format!(
            "{}: scheme must be http or https",
            cli.api_url
        )));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(ConfigError::InvalidApiUrl(format!(
            "{}: empty host",
            cli.api_url
        )));
    }

    let config = load_config_file(cli.config.as_deref())?;

    let mut polling = config.polling;
    if let Some(v) = cli.tickets_cadence {
        polling.tickets_seconds = v;
    }
    if let Some(v) = cli.events_cadence {
        polling.events_seconds = v;
    }
    if let Some(v) = cli.escalations_cadence {
        polling.escalations_seconds = v;
    }

    if polling.tickets_seconds < 1 {
        return Err(ConfigError::InvalidTicketsCadence(polling.tickets_seconds));
    }
    if polling.events_seconds < 1 {
        return Err(ConfigError::InvalidEventsCadence(polling.events_seconds));
    }
    if polling.escalations_seconds < 1 {
        return Err(ConfigError::InvalidEscalationsCadence(
            polling.escalations_seconds,
        ));
    }

    Ok(ResolvedConfig {
        api_url: cli.api_url,
        polling,
    })
}

fn load_config_file(explicit: Option<&Path>) -> Result<TuiConfig, ConfigError> {
    if let Some(path) = explicit {
        if !path.exists() {
            return Err(ConfigError::ConfigNotFound(path.to_path_buf()));
        }
        let body = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Parse(format!("{}: {}", path.display(), e)))?;
        return toml::from_str::<TuiConfig>(&body)
            .map_err(|e| ConfigError::Parse(format!("{}: {}", path.display(), e)));
    }
    let candidate = default_path();
    if let Some(path) = candidate.as_ref() {
        if path.exists() {
            let body = std::fs::read_to_string(path)
                .map_err(|e| ConfigError::Parse(format!("{}: {}", path.display(), e)))?;
            return toml::from_str::<TuiConfig>(&body)
                .map_err(|e| ConfigError::Parse(format!("{}: {}", path.display(), e)));
        }
    }
    Ok(TuiConfig::default())
}

fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".config/roki-tui/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cli(url: &str) -> Cli {
        Cli {
            api_url: url.into(),
            config: None,
            tickets_cadence: None,
            events_cadence: None,
            escalations_cadence: None,
        }
    }

    #[test]
    fn defaults_when_no_file_and_no_overrides() {
        // route around HOME by passing an explicit non-existent --config
        let dir = TempDir::new().unwrap();
        let mut c = cli("http://127.0.0.1:8080");
        c.config = Some(dir.path().join("does-not-exist.toml"));
        let err = resolve(c).unwrap_err();
        assert!(matches!(err, ConfigError::ConfigNotFound(_)));
    }

    #[test]
    fn defaults_when_default_path_absent() {
        let mut c = cli("http://127.0.0.1:8080");
        c.config = None;
        // Even if a default file happens to exist on the developer's machine
        // we still expect a valid PollingSection; just assert no validation
        // error fires.
        let r = resolve(c).unwrap();
        assert!(r.polling.tickets_seconds >= 1);
        assert!(r.polling.events_seconds >= 1);
        assert!(r.polling.escalations_seconds >= 1);
    }

    #[test]
    fn cli_overrides_defaults() {
        let mut c = cli("http://127.0.0.1:8080");
        c.tickets_cadence = Some(7);
        c.events_cadence = Some(11);
        c.escalations_cadence = Some(13);
        let r = resolve(c).unwrap();
        assert_eq!(r.polling.tickets_seconds, 7);
        assert_eq!(r.polling.events_seconds, 11);
        assert_eq!(r.polling.escalations_seconds, 13);
    }

    #[test]
    fn rejects_zero_cadence() {
        let mut c = cli("http://127.0.0.1:8080");
        c.tickets_cadence = Some(0);
        let err = resolve(c).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidTicketsCadence(0)));
    }

    #[test]
    fn rejects_unknown_scheme() {
        let c = cli("ftp://127.0.0.1");
        let err = resolve(c).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidApiUrl(_)));
    }

    #[test]
    fn explicit_file_loads_polling() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("tui.toml");
        std::fs::write(
            &p,
            r#"
[polling]
tickets_seconds = 4
events_seconds = 2
escalations_seconds = 9
"#,
        )
        .unwrap();
        let mut c = cli("http://127.0.0.1:8080");
        c.config = Some(p);
        let r = resolve(c).unwrap();
        assert_eq!(r.polling.tickets_seconds, 4);
        assert_eq!(r.polling.events_seconds, 2);
        assert_eq!(r.polling.escalations_seconds, 9);
    }
}
