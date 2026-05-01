//! Command-line surface for the `roki` binary.
//!
//! Task 1.1 introduced the parser shell. Task 5.1 extends [`RunArgs`] with the
//! bootstrap flags documented in `.kiro/specs/roki-mvp/design-bootstrap.md`
//! (config-path override, server bind/port overrides, dangerous-permissions
//! override). Precedence: CLI flags override `[server]` / `[permission_strategy]`
//! values from the config file.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Top-level CLI for the roki daemon.
#[derive(Debug, Parser)]
#[command(
    name = "roki",
    version,
    about = "roki daemon: Linear-driven, per-issue agent orchestrator",
    long_about = "roki observes Linear, allocates per-(repo, issue) workspaces, \
                  and supervises long-lived Claude Code subprocesses. The daemon \
                  never writes Linear, never opens PRs, and never edits code; \
                  the agent does all of that inside its sandbox."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands recognized by the daemon.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon (orchestrator, Linear adapter, workflow loader).
    Run(RunArgs),
}

/// Arguments for the `roki run` subcommand.
///
/// Task 5.1 introduces the full bootstrap flag surface:
///
/// * `--config <path>` — path to the daemon config file. When omitted the
///   daemon loads `./roki.toml`. An explicit but missing path is a hard error.
/// * `--bind <addr>` — override `[server].bind` from the config file. Accepts
///   any IPv4/IPv6 literal that resolves under `IpAddr::from_str`.
/// * `--port <num>` — override `[server].port` from the config file. Must be
///   non-zero.
/// * `--dangerously-skip-permissions` — override `[permission_strategy]` to the
///   dangerous fallback regardless of what the file declares. The
///   [`crate::permissions::PermissionResolver`] still emits a per-launch WARN
///   log, so the elevated-permission decision is auditable.
///
/// CLI flags override the config file when both are present. The defaults
/// applied only when *both* the file value and the CLI override are absent
/// are documented on the matching constants in [`crate::config`].
#[derive(Debug, Default, Parser)]
pub struct RunArgs {
    /// Path to the daemon config file. Defaults to `./roki.toml` when
    /// omitted.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Override `[server].bind` from the config file (default `127.0.0.1`).
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<IpAddr>,

    /// Override `[server].port` from the config file (default `7878`).
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Force the dangerous-skip-permissions fallback regardless of the
    /// config-file permission strategy. Every worker launch emits a WARN log.
    #[arg(long)]
    pub dangerously_skip_permissions: bool,
}

impl Cli {
    /// Parse arguments from `std::env::args_os`.
    ///
    /// Errors and `--help` output are handled by clap; this wrapper exists so
    /// `main` and integration tests share a single entry point.
    pub fn parse_from_env() -> Self {
        Self::parse()
    }
}
