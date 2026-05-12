//! Resolve `session_root`, API URL, and ticket/cycle identifiers from
//! environment variables (preferred — lets a parent daemon inject context)
//! and `--config` (fallback).

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::roki::RokiConfig;
use crate::error::RokiConfigError;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("cannot resolve session_root (set --config or run from a state subprocess)")]
    NoSessionRoot,
    #[error("cannot resolve API URL (set --api, ROKI_API_URL, or --config with [api])")]
    NoApiUrl,
    #[error("ticket missing (pass --ticket or set ROKI_TICKET_ID)")]
    MissingTicket,
    #[error("cycle missing (pass --cycle or set ROKI_CYCLE_ID)")]
    MissingCycle,
    #[error("cross-ticket read refused")]
    CrossTicketRefused,
    #[error("config load failed: {0}")]
    LoadConfig(#[from] RokiConfigError),
}

/// Resolve `session_root` from `ROKI_CONFIG_SESSION_ROOT` (preferred,
/// so parent daemon processes can inject the path without children
/// re-parsing `roki.toml`) or, when unset, from `[paths].session_root`
/// in the config file at `config_path`.
pub fn resolve_session_root(config_path: Option<&Path>) -> Result<PathBuf, ResolveError> {
    if let Ok(s) = std::env::var("ROKI_CONFIG_SESSION_ROOT")
        && !s.is_empty()
    {
        return Ok(PathBuf::from(s));
    }
    let path = config_path.ok_or(ResolveError::NoSessionRoot)?;
    let cfg = RokiConfig::load(path)?;
    Ok(cfg.paths.session_root)
}

/// Resolve the API base URL. Precedence: explicit `--api` flag, then
/// `ROKI_API_URL`, then `[api]` from the loaded config (`http://{bind}:{port}`).
pub fn resolve_api_url(
    flag: Option<&str>,
    config_path: Option<&Path>,
) -> Result<String, ResolveError> {
    if let Some(s) = flag {
        return Ok(s.to_string());
    }
    if let Ok(s) = std::env::var("ROKI_API_URL")
        && !s.is_empty()
    {
        return Ok(s);
    }
    let path = config_path.ok_or(ResolveError::NoApiUrl)?;
    let cfg = RokiConfig::load(path)?;
    let port = cfg.api.port.ok_or(ResolveError::NoApiUrl)?;
    // `cfg.api.bind` is a non-`Option` String; fall back only when
    // the config supplied an empty value.
    let bind = if cfg.api.bind.is_empty() {
        "127.0.0.1".to_string()
    } else {
        cfg.api.bind
    };
    Ok(format!("http://{bind}:{port}"))
}

/// Resolve ticket and cycle IDs from the matching CLI flag first, then
/// from `ROKI_TICKET_ID` / `ROKI_CYCLE_ID` env vars.
pub fn resolve_ticket_and_cycle(
    ticket_flag: Option<&str>,
    cycle_flag: Option<&str>,
) -> Result<(String, String), ResolveError> {
    let ticket = ticket_flag
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var("ROKI_TICKET_ID")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .ok_or(ResolveError::MissingTicket)?;
    let cycle = cycle_flag
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::var("ROKI_CYCLE_ID")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .ok_or(ResolveError::MissingCycle)?;
    Ok((ticket, cycle))
}

/// Refuse a read whose `--ticket` flag disagrees with the
/// `ROKI_TICKET_ID` already injected by the parent daemon — this is
/// the cross-ticket guard that prevents a state-subprocess invocation
/// from peeking at a different ticket's events.
pub fn enforce_same_ticket(flag: Option<&str>) -> Result<(), ResolveError> {
    if let (Some(flag_val), Ok(env_val)) = (flag, std::env::var("ROKI_TICKET_ID"))
        && !env_val.is_empty()
        && flag_val != env_val
    {
        return Err(ResolveError::CrossTicketRefused);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn env_wins_over_config_path() {
        let result = temp_env::with_var("ROKI_CONFIG_SESSION_ROOT", Some("/from/env"), || {
            resolve_session_root(None).unwrap()
        });
        assert_eq!(result, PathBuf::from("/from/env"));
    }

    #[test]
    fn config_path_used_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("roki.toml");
        std::fs::write(
            &toml,
            r#"
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
session_root = "/from/toml"
[log]
"#,
        )
        .unwrap();
        let result = temp_env::with_var_unset("ROKI_CONFIG_SESSION_ROOT", || {
            resolve_session_root(Some(&toml)).unwrap()
        });
        assert_eq!(result, PathBuf::from("/from/toml"));
    }

    #[test]
    fn errors_when_neither_env_nor_config() {
        let err = temp_env::with_var_unset("ROKI_CONFIG_SESSION_ROOT", || {
            resolve_session_root(None).unwrap_err()
        });
        assert!(format!("{err}").contains("cannot resolve session_root"));
    }

    #[test]
    fn api_url_flag_beats_env() {
        let url = temp_env::with_var("ROKI_API_URL", Some("http://from-env"), || {
            resolve_api_url(Some("http://from-flag"), None).unwrap()
        });
        assert_eq!(url, "http://from-flag");
    }

    #[test]
    fn api_url_env_beats_config() {
        let url = temp_env::with_var("ROKI_API_URL", Some("http://from-env"), || {
            resolve_api_url(None, None).unwrap()
        });
        assert_eq!(url, "http://from-env");
    }

    #[test]
    fn api_url_errors_when_nothing_resolves() {
        let err =
            temp_env::with_var_unset("ROKI_API_URL", || resolve_api_url(None, None).unwrap_err());
        assert!(matches!(err, ResolveError::NoApiUrl));
    }

    #[test]
    fn enforce_same_ticket_passes_when_flag_matches_env() {
        let r = temp_env::with_var("ROKI_TICKET_ID", Some("ABC-1"), || {
            enforce_same_ticket(Some("ABC-1"))
        });
        assert!(r.is_ok());
    }

    #[test]
    fn enforce_same_ticket_refuses_mismatch() {
        let err = temp_env::with_var("ROKI_TICKET_ID", Some("ABC-1"), || {
            enforce_same_ticket(Some("XYZ-9")).unwrap_err()
        });
        assert!(format!("{err}").contains("cross-ticket read refused"));
    }
}
