//! CLI surface for `roki-tui`. Resolves the positional API URL and the three
//! optional cadence overrides. CLI values override the TOML config file.

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(name = "roki-tui", about = "Terminal UI for the roki daemon HTTP API")]
pub struct Cli {
    /// Base URL of the roki HTTP API (e.g. http://127.0.0.1:8080)
    pub api_url: String,

    /// Override the TOML config file location.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Override [polling].tickets_seconds.
    #[arg(long)]
    pub tickets_cadence: Option<u32>,

    /// Override [polling].events_seconds.
    #[arg(long)]
    pub events_cadence: Option<u32>,

    /// Override [polling].escalations_seconds.
    #[arg(long)]
    pub escalations_cadence: Option<u32>,
}

impl Cli {
    pub fn parse_args() -> Self {
        <Self as Parser>::parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_positional_api_url() {
        let cli = Cli::try_parse_from(["roki-tui", "http://127.0.0.1:8080"]).unwrap();
        assert_eq!(cli.api_url, "http://127.0.0.1:8080");
        assert!(cli.tickets_cadence.is_none());
    }

    #[test]
    fn parses_cadence_overrides() {
        let cli = Cli::try_parse_from([
            "roki-tui",
            "http://x",
            "--tickets-cadence",
            "3",
            "--events-cadence",
            "2",
            "--escalations-cadence",
            "10",
        ])
        .unwrap();
        assert_eq!(cli.tickets_cadence, Some(3));
        assert_eq!(cli.events_cadence, Some(2));
        assert_eq!(cli.escalations_cadence, Some(10));
    }

    #[test]
    fn rejects_missing_api_url() {
        let r = Cli::try_parse_from(["roki-tui"]);
        assert!(r.is_err());
    }
}
