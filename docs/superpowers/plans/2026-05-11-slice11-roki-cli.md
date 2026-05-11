# Slice 11 `roki log` / `roki events` / `roki repo` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `fr:09` end to end inside the `roki` binary — three new subcommands (`log`, `events`, `repo`) plus the daemon-side env injection they assume.

**Architecture:** New module tree `crates/roki-daemon/src/cli/{log,events,repo,shared,workflow}.rs` (the current single-file `cli.rs` is split into a `cli/` directory at the top of the slice so each subcommand sits in its own file). `cli::log` reads the existing on-disk visit layout via the shared `api::projection::visits` helper. `cli::events` is an async `reqwest` client over `/api/events`, falling back to a JSON Lines file reader in `--offline`. `cli::repo` is a thin wrapper over the already-published `engine::cwd::resolve` / `resolve_ghq_base`. Three daemon-side env hooks (`ROKI_CONFIG_SESSION_ROOT`, `ROKI_API_URL`, fr:09 doc patches) land first so the CLIs can rely on them.

**Tech Stack:** Rust 2024 (workspace edition). Existing daemon deps cover everything except: `reqwest` (already in workspace; add to `roki-daemon` features = `["json", "rustls-tls"]`). Test deps reused: `tempfile`, `temp_env`, `wiremock` (already used elsewhere in the workspace by `roki-tui` — add to `roki-daemon` dev-deps for online HTTP fixtures).

**Spec:** `docs/superpowers/specs/2026-05-11-slice11-roki-cli-design.md`.

**Working branch:** `feature/slice11-roki-cli` (already created; spec committed). Every implementation commit lands here.

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-daemon/src/cli/mod.rs` | Parser root, `run()` dispatcher (replaces today's `src/cli.rs`). |
| `crates/roki-daemon/src/cli/workflow.rs` | Existing `workflow validate|graph` (extracted from today's `src/cli.rs`). |
| `crates/roki-daemon/src/cli/log.rs` | `roki log` subcommand: arg parser + behavior. |
| `crates/roki-daemon/src/cli/events.rs` | `roki events` subcommand: arg parser + online + offline paths. |
| `crates/roki-daemon/src/cli/repo.rs` | `roki repo` subcommand. |
| `crates/roki-daemon/src/cli/shared/mod.rs` | Re-exports for `config_resolve`, `visit_lookup`, `tail`, `sanitize`. |
| `crates/roki-daemon/src/cli/shared/config_resolve.rs` | `--config` loader fragment + env-var fallback helpers. |
| `crates/roki-daemon/src/cli/shared/visit_lookup.rs` | Visit-dir enumeration, absolute / relative `--iter` resolution. |
| `crates/roki-daemon/src/cli/shared/tail.rs` | Line-tail (`tail_lines`) + byte-tail (`tail_bytes`) readers. |
| `crates/roki-daemon/src/cli/shared/sanitize.rs` | `strip_ansi_terminal` wrapper around the published `api::sanitize::strip_ansi`. |
| `crates/roki-daemon/src/cli/shared/events_format.rs` | `format_human` reformatter for `ApiEvent`. |
| `crates/roki-daemon/tests/e2e/cli_log_smoke.rs` | E2E: stream read against a real fixture cycle directory. |
| `crates/roki-daemon/tests/e2e/cli_log_follow.rs` | E2E: `--follow` sees late appends. |
| `crates/roki-daemon/tests/e2e/cli_events_online_smoke.rs` | E2E: HTTP `--tail` against `wiremock`. |
| `crates/roki-daemon/tests/e2e/cli_events_offline_smoke.rs` | E2E: JSON Lines file reader. |
| `crates/roki-daemon/tests/e2e/cli_repo_smoke.rs` | E2E: worktree-present, worktree-absent, `--worktree` strict failure. |

### Modified

| Path | Why |
|---|---|
| `crates/roki-daemon/src/cli.rs` | Deleted in Task 2 (content moves into `cli/mod.rs` + `cli/workflow.rs`). |
| `crates/roki-daemon/src/lib.rs` | Add `pub mod cli;` (replacing today's `pub mod cli;` re-export — same path, now resolves to the directory). |
| `crates/roki-daemon/src/main.rs` | No change beyond verifying `cli::run().await` still compiles. |
| `crates/roki-daemon/src/daemon/real_runner.rs` | Add `session_root` and (conditionally) `api_url` scalars to `globals.config`. |
| `crates/roki-daemon/src/engine/real_state_runner.rs` | Update names-contains test to assert `ROKI_CONFIG_SESSION_ROOT` and (when `[api].port` set) `ROKI_API_URL`. |
| `crates/roki-daemon/src/engine/cwd.rs` | Drop `#![allow(dead_code)]` once `cli::repo` calls `resolve` / `resolve_ghq_base`. |
| `crates/roki-daemon/src/api/sanitize.rs` | Promote `strip_ansi` from private fn to `pub fn` so `cli::shared::sanitize` reuses it without re-implementing the VTE walker. |
| `crates/roki-daemon/Cargo.toml` | Add `reqwest` to `[dependencies]`, `wiremock` to `[dev-dependencies]`, register new `[[test]]` entries. |
| `docs/fr/09-log-access-cli.md` | Patch `$ROKI_REPO` → `$ROKI_REPO_GHQ`; `meta.json` → `cycle.json`; document `$ROKI_API_URL` / `--api`, `$ROKI_CONFIG_SESSION_ROOT` / `--config`. |
| `docs/reference/cli.md` | Add canonical flag tables for `roki log` / `roki events` / `roki repo`. |

---

## Task 0: Confirm spec + branch

- [ ] **Step 1: Confirm branch + clean tree**

```bash
git rev-parse --abbrev-ref HEAD
git status --short
```

Expected: `feature/slice11-roki-cli` and a clean working tree (the spec has been committed).

- [ ] **Step 2: Confirm spec exists and validates**

```bash
ls -la docs/superpowers/specs/2026-05-11-slice11-roki-cli-design.md
kusara validate
```

Expected: file present; `OK (22 docs)` (or higher count after later doc edits — the count is informational).

- [ ] **Step 3: Confirm baseline test suite is green**

```bash
cargo test --workspace --no-run
cargo test --workspace 2>&1 | tail -20
```

Expected: all crates compile; all existing tests pass. Stop and fix any failure before continuing — slice 11 must build on a green baseline.

---

## Task 1: Inject `ROKI_CONFIG_SESSION_ROOT` and `ROKI_API_URL`

Daemon-side prerequisite (spec §3.1, §3.2). Without these env vars the CLI subcommands cannot resolve defaults inside a state subprocess.

**Files:**
- Modify: `crates/roki-daemon/src/daemon/real_runner.rs:312-339` (the `globals.config` insert in `build_cycle_context`).
- Modify: `crates/roki-daemon/src/engine/real_state_runner.rs:957` (the existing `ROKI_TICKET_ID` names-contains test).

- [ ] **Step 1: Write the failing test for `session_root` injection**

Add to `crates/roki-daemon/src/daemon/real_runner.rs` `mod tests` (locate the nearest existing `mod tests` block; otherwise add one):

```rust
#[test]
fn build_cycle_context_exports_session_root_into_globals_config() {
    let cfg = RokiConfig::test_default(std::path::Path::new("/tmp/sess-x"));
    let admitted = AdmittedTicket {
        ticket: TicketDetail {
            id: "ENG-1".into(),
            title: "t".into(),
            body: "b".into(),
            labels: vec![],
            assignee_id: None,
            status: "Backlog".into(),
        },
        ghq: "github.com/x/y".into(),
    };
    let cx = build_cycle_context(
        &cfg,
        &admitted,
        Uuid::nil(),
        CycleKind::Rule,
        CycleTrigger::Runtime,
        None,
    );
    let cfg_obj = cx
        .globals
        .get("config")
        .and_then(|v| v.as_object())
        .expect("config namespace present");
    assert_eq!(
        cfg_obj.get("session_root").and_then(|v| v.as_str()),
        Some("/tmp/sess-x")
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p roki-daemon --lib build_cycle_context_exports_session_root_into_globals_config -- --nocapture
```

Expected: FAIL — `assertion failed: ... Some("/tmp/sess-x")` (the key is missing).

- [ ] **Step 3: Implement the `session_root` insert**

Replace the `globals.insert("config", ...)` block in `crates/roki-daemon/src/daemon/real_runner.rs` with:

```rust
globals.insert(
    "config".into(),
    serde_json::json!({
        "max_iterations": cfg.engine.max_iterations,
        "session_root": cfg.paths.session_root.to_string_lossy(),
    }),
);
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test -p roki-daemon --lib build_cycle_context_exports_session_root_into_globals_config -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Write the failing test for `api_url` injection**

Append to the same `mod tests`:

```rust
#[test]
fn build_cycle_context_exports_api_url_when_port_set() {
    let mut cfg = RokiConfig::test_default(std::path::Path::new("/tmp/sess-x"));
    cfg.api.port = Some(7777);
    // bind defaults to 127.0.0.1 in test_default; verify the synthesized URL.
    let admitted = AdmittedTicket {
        ticket: TicketDetail {
            id: "ENG-1".into(),
            title: "t".into(),
            body: "b".into(),
            labels: vec![],
            assignee_id: None,
            status: "Backlog".into(),
        },
        ghq: "github.com/x/y".into(),
    };
    let cx = build_cycle_context(
        &cfg,
        &admitted,
        Uuid::nil(),
        CycleKind::Rule,
        CycleTrigger::Runtime,
        None,
    );
    let url = cx
        .globals
        .get("config")
        .and_then(|v| v.get("api_url"))
        .and_then(|v| v.as_str())
        .expect("api_url present");
    assert_eq!(url, "http://127.0.0.1:7777");
}

#[test]
fn build_cycle_context_omits_api_url_when_port_unset() {
    let cfg = RokiConfig::test_default(std::path::Path::new("/tmp/sess-x"));
    assert!(cfg.api.port.is_none());
    let admitted = AdmittedTicket {
        ticket: TicketDetail {
            id: "ENG-1".into(),
            title: "t".into(),
            body: "b".into(),
            labels: vec![],
            assignee_id: None,
            status: "Backlog".into(),
        },
        ghq: "github.com/x/y".into(),
    };
    let cx = build_cycle_context(
        &cfg,
        &admitted,
        Uuid::nil(),
        CycleKind::Rule,
        CycleTrigger::Runtime,
        None,
    );
    let cfg_obj = cx
        .globals
        .get("config")
        .and_then(|v| v.as_object())
        .unwrap();
    assert!(cfg_obj.get("api_url").is_none());
}
```

- [ ] **Step 6: Run the tests to verify they fail**

```bash
cargo test -p roki-daemon --lib build_cycle_context_exports_api_url -- --nocapture
cargo test -p roki-daemon --lib build_cycle_context_omits_api_url -- --nocapture
```

Expected: the `_exports_` test FAILs (key missing); the `_omits_` test PASSes (key is already absent).

- [ ] **Step 7: Implement `api_url` insert**

Below the existing `globals.insert("config", ...)` block in `build_cycle_context`, add:

```rust
if let Some(port) = cfg.api.port {
    let bind = cfg
        .api
        .bind
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    if let Some(serde_json::Value::Object(m)) = globals.get_mut("config") {
        m.insert(
            "api_url".into(),
            serde_json::Value::String(format!("http://{bind}:{port}")),
        );
    }
}
```

If `cfg.api.bind` is a different type (e.g., `Option<IpAddr>` vs `Option<String>`), adjust the `.map(...)` accordingly — the goal is the literal string `127.0.0.1` or whatever the operator configured.

- [ ] **Step 8: Run the tests to verify they pass**

```bash
cargo test -p roki-daemon --lib build_cycle_context -- --nocapture
```

Expected: all three new tests PASS.

- [ ] **Step 9: Update the env-pairs names test**

In `crates/roki-daemon/src/engine/real_state_runner.rs:957` (the test that asserts `names.contains(&"ROKI_TICKET_ID")`), extend with two assertions:

```rust
assert!(names.contains(&"ROKI_CONFIG_SESSION_ROOT"));
// ROKI_API_URL is only set when the test fixture configures [api].port.
// The default fixture leaves the API server disabled, so assert absence
// to lock the gating behavior.
assert!(!names.contains(&"ROKI_API_URL"));
```

If the fixture in this test path constructs a `RokiConfig` that does set `api.port`, mirror the assertion to require `ROKI_API_URL` present instead.

- [ ] **Step 10: Run the env-pairs test to verify it passes**

```bash
cargo test -p roki-daemon --lib real_state_runner -- --nocapture
```

Expected: PASS.

- [ ] **Step 11: Commit**

```bash
git add crates/roki-daemon/src/daemon/real_runner.rs \
        crates/roki-daemon/src/engine/real_state_runner.rs
git commit -m "$(cat <<'EOF'
feat(slice11): inject ROKI_CONFIG_SESSION_ROOT and ROKI_API_URL

Adds paths.session_root and (when [api].port is set) the synthesized
API URL to globals.config so the existing scalar flattener exports
them as env vars to every state subprocess. Prerequisite for
roki log / roki events to resolve defaults inside a state subprocess.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Split `cli.rs` into `cli/` module directory

Pure refactor with zero behavior change. Done before adding new subcommands so each one lands in its own file.

**Files:**
- Delete: `crates/roki-daemon/src/cli.rs`.
- Create: `crates/roki-daemon/src/cli/mod.rs`, `crates/roki-daemon/src/cli/workflow.rs`.

- [ ] **Step 1: Capture the existing entry point**

```bash
cat crates/roki-daemon/src/cli.rs | head -120
```

Read end-to-end so you have the full `Cli` / `CliCommand` / `WorkflowCmd` / `GraphFormat` / `run` / `workflow_validate` / `workflow_graph` content in front of you before splitting.

- [ ] **Step 2: Create `cli/mod.rs`**

```rust
//! CLI parser for the roki binary.
//!
//! Top-level `roki` command exposes `run`, `cleanup`, `workflow`, and the
//! slice-11 subcommands (`log`, `events`, `repo`). [`run`] parses argv,
//! dispatches the matched subcommand, and returns an [`ExitCode`]
//! propagated by `main`.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::runtime;

pub mod workflow;
// pub mod log;     // wired in Task 7
// pub mod events;  // wired in Task 10
// pub mod repo;    // wired in Task 6
// pub mod shared;  // wired in Task 3

/// roki — Linear-driven coding-agent daemon.
#[derive(Debug, Parser)]
#[command(name = "roki", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Start the daemon with default dispatch (cleanup-first then rule).
    Run {
        /// Path to the roki.toml configuration file.
        #[arg(long = "config", value_name = "PATH")]
        config: PathBuf,
    },
    /// Cleanup-only dispatch: only [[cleanup]] matches lead to a cycle.
    Cleanup {
        #[arg(long = "config", value_name = "PATH")]
        config: PathBuf,
    },
    /// Workflow YAML utilities.
    Workflow {
        #[command(subcommand)]
        cmd: workflow::WorkflowCmd,
    },
}

pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Run { config } => runtime::run(&config, runtime::DispatchMode::Default).await,
        CliCommand::Cleanup { config } => {
            runtime::run(&config, runtime::DispatchMode::CleanupOnly).await
        }
        CliCommand::Workflow { cmd } => workflow::dispatch(cmd),
    }
}
```

- [ ] **Step 3: Create `cli/workflow.rs`**

Move every workflow-related item out of the old `cli.rs` and into this file. The full content (read from the source you captured in Step 1):

```rust
//! `roki workflow` subcommands.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Subcommand;

use crate::workflow::{parse, sugar};

#[derive(Debug, Subcommand)]
pub enum WorkflowCmd {
    /// Load + sugar-expand + validate a WORKFLOW.yaml file.
    Validate {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Render a rule's state machine as ASCII or DOT.
    Graph {
        #[arg(value_name = "FILE")]
        file: PathBuf,
        #[arg(long = "rule", value_name = "SELECTOR")]
        rule: Option<String>,
        #[arg(long = "format", value_name = "FORMAT", default_value = "ascii")]
        format: GraphFormat,
        #[arg(long = "out", value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum GraphFormat {
    Ascii,
    Dot,
}

pub fn dispatch(cmd: WorkflowCmd) -> ExitCode {
    match cmd {
        WorkflowCmd::Validate { file } => workflow_validate(&file),
        WorkflowCmd::Graph {
            file,
            rule,
            format,
            out,
        } => workflow_graph(&file, rule.as_deref(), format, out.as_deref()),
    }
}

fn workflow_validate(file: &std::path::Path) -> ExitCode {
    // ... move the existing body verbatim from cli.rs ...
}

fn workflow_graph(
    file: &std::path::Path,
    rule: Option<&str>,
    format: GraphFormat,
    out: Option<&std::path::Path>,
) -> ExitCode {
    // ... move the existing body verbatim from cli.rs ...
}

#[cfg(test)]
mod tests {
    // ... move any existing cli.rs tests that exercise workflow_validate /
    // workflow_graph into this file unchanged ...
}
```

When transcribing the function bodies, copy them byte-for-byte from the old `cli.rs`. Update no logic.

- [ ] **Step 4: Delete the old `cli.rs`**

```bash
git rm crates/roki-daemon/src/cli.rs
```

- [ ] **Step 5: Verify the module path resolves**

```bash
grep -n "pub mod cli" crates/roki-daemon/src/lib.rs
```

Expected: the existing `pub mod cli;` line is unchanged — it now resolves to the directory.

- [ ] **Step 6: Run the daemon test suite to verify zero behavior change**

```bash
cargo test -p roki-daemon --lib 2>&1 | tail -20
cargo test -p roki-daemon --test workflow_graph_cli_smoke 2>&1 | tail -10
```

Expected: every test that passed in Task 0 still passes.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/src/cli/
git commit -m "$(cat <<'EOF'
refactor(slice11): split cli.rs into cli/ module directory

Pure code move; no behavior change. cli/workflow.rs holds the
existing workflow validate|graph; cli/mod.rs holds the parser root
and run() dispatcher. Subsequent slice-11 tasks add log/events/repo
modules alongside workflow.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `cli::shared::config_resolve` + `visit_lookup` + `tail`

Shared building blocks every subcommand depends on. TDD each.

**Files:**
- Create: `crates/roki-daemon/src/cli/shared/mod.rs`, `crates/roki-daemon/src/cli/shared/config_resolve.rs`, `crates/roki-daemon/src/cli/shared/visit_lookup.rs`, `crates/roki-daemon/src/cli/shared/tail.rs`.
- Modify: `crates/roki-daemon/src/cli/mod.rs` (wire `pub mod shared;`).

### 3.1 `shared::mod.rs`

- [ ] **Step 1: Create the module barrel**

```rust
//! Building blocks shared by `roki log`, `roki events`, `roki repo`.

pub mod config_resolve;
pub mod visit_lookup;
pub mod tail;
```

Then uncomment `pub mod shared;` in `cli/mod.rs`.

### 3.2 `config_resolve`

- [ ] **Step 2: Write the failing test for `resolve_session_root`**

`crates/roki-daemon/src/cli/shared/config_resolve.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn env_wins_over_config_path() {
        let result = temp_env::with_var(
            "ROKI_CONFIG_SESSION_ROOT",
            Some("/from/env"),
            || resolve_session_root(None).unwrap(),
        );
        assert_eq!(result, PathBuf::from("/from/env"));
    }

    #[test]
    fn config_path_used_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("roki.toml");
        std::fs::write(
            &toml,
            "[paths]\nworkflow=\"w\"\nsession_root=\"/from/toml\"\n\
             [linear.poll]\ninterval_seconds=60\n[engine]\nmax_iterations=10\n\
             [linear.webhook]\nbind=\"127.0.0.1\"\nport=9000\n",
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
}
```

(Adjust the minimal `roki.toml` body in the second test to whatever today's parser accepts — copy from the closest existing parser fixture in `crates/roki-daemon/src/config/roki.rs::tests`.)

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cargo test -p roki-daemon --lib cli::shared::config_resolve 2>&1 | tail -10
```

Expected: compile failure (`resolve_session_root` undefined).

- [ ] **Step 4: Implement `resolve_session_root`**

Above the `#[cfg(test)] mod tests` block:

```rust
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::roki::RokiConfig;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("cannot resolve session_root (set --config or run from a state subprocess)")]
    NoSessionRoot,
    #[error("cannot resolve API URL (set --api, ROKI_API_URL, or --config with [api])")]
    NoApiUrl,
    #[error("config error: {0}")]
    Config(String),
}

pub fn resolve_session_root(config_path: Option<&Path>) -> Result<PathBuf, ResolveError> {
    if let Ok(s) = std::env::var("ROKI_CONFIG_SESSION_ROOT") {
        if !s.is_empty() {
            return Ok(PathBuf::from(s));
        }
    }
    let path = config_path.ok_or(ResolveError::NoSessionRoot)?;
    let cfg = RokiConfig::load(path).map_err(|e| ResolveError::Config(format!("{e}")))?;
    Ok(cfg.paths.session_root)
}

pub fn resolve_api_url(
    flag: Option<&str>,
    config_path: Option<&Path>,
) -> Result<String, ResolveError> {
    if let Some(s) = flag {
        return Ok(s.to_string());
    }
    if let Ok(s) = std::env::var("ROKI_API_URL") {
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let path = config_path.ok_or(ResolveError::NoApiUrl)?;
    let cfg = RokiConfig::load(path).map_err(|e| ResolveError::Config(format!("{e}")))?;
    let port = cfg.api.port.ok_or(ResolveError::NoApiUrl)?;
    let bind = cfg
        .api
        .bind
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    Ok(format!("http://{bind}:{port}"))
}

pub fn resolve_ticket_and_cycle(
    ticket_flag: Option<&str>,
    cycle_flag: Option<&str>,
) -> Result<(String, String), ResolveError> {
    let ticket = ticket_flag
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ROKI_TICKET_ID").ok().filter(|s| !s.is_empty()))
        .ok_or_else(|| ResolveError::Config("ticket missing".into()))?;
    let cycle = cycle_flag
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ROKI_CYCLE_ID").ok().filter(|s| !s.is_empty()))
        .ok_or_else(|| ResolveError::Config("cycle missing".into()))?;
    Ok((ticket, cycle))
}

pub fn enforce_same_ticket(flag: Option<&str>) -> Result<(), ResolveError> {
    if let (Some(flag_val), Ok(env_val)) = (flag, std::env::var("ROKI_TICKET_ID"))
        && !env_val.is_empty()
        && flag_val != env_val
    {
        return Err(ResolveError::Config(
            "cross-ticket read refused".into(),
        ));
    }
    Ok(())
}
```

If `RokiConfig::load(path)` is not the actual function name, replace with whatever loader the daemon uses (search `crates/roki-daemon/src/config/roki.rs` for `pub fn load` or similar).

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test -p roki-daemon --lib cli::shared::config_resolve 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 6: Add tests for `resolve_api_url` and `enforce_same_ticket`**

Append to the `mod tests`:

```rust
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
    let err = temp_env::with_var_unset("ROKI_API_URL", || {
        resolve_api_url(None, None).unwrap_err()
    });
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
```

- [ ] **Step 7: Run all `config_resolve` tests**

```bash
cargo test -p roki-daemon --lib cli::shared::config_resolve 2>&1 | tail -10
```

Expected: every test PASSes.

### 3.3 `visit_lookup`

- [ ] **Step 8: Write failing tests**

`crates/roki-daemon/src/cli/shared/visit_lookup.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_cycle(visits: &[u32]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for n in visits {
            std::fs::create_dir_all(dir.path().join(format!("visit-{n:03}"))).unwrap();
        }
        dir
    }

    #[test]
    fn lists_visits_sorted_ascending() {
        let d = fixture_cycle(&[3, 1, 2]);
        let v = list_visits(d.path()).unwrap();
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn resolve_absolute_iter() {
        let d = fixture_cycle(&[1, 2, 3]);
        assert_eq!(resolve_iter(d.path(), Some(2)).unwrap(), 2);
    }

    #[test]
    fn resolve_negative_iter_takes_n_back_from_last() {
        let d = fixture_cycle(&[1, 2, 3]);
        assert_eq!(resolve_iter(d.path(), Some(-1)).unwrap(), 3);
        assert_eq!(resolve_iter(d.path(), Some(-2)).unwrap(), 2);
    }

    #[test]
    fn resolve_iter_off_the_start_errors() {
        let d = fixture_cycle(&[1, 2]);
        assert!(resolve_iter(d.path(), Some(-5)).is_err());
    }

    #[test]
    fn resolve_iter_none_returns_latest() {
        let d = fixture_cycle(&[5, 1, 9]);
        assert_eq!(resolve_iter(d.path(), None).unwrap(), 9);
    }

    #[test]
    fn missing_absolute_iter_errors() {
        let d = fixture_cycle(&[1, 2]);
        assert!(resolve_iter(d.path(), Some(7)).is_err());
    }
}
```

- [ ] **Step 9: Run to verify failure**

```bash
cargo test -p roki-daemon --lib cli::shared::visit_lookup 2>&1 | tail -10
```

Expected: compile failure (`list_visits`, `resolve_iter` undefined).

- [ ] **Step 10: Implement**

```rust
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VisitError {
    #[error("visit-{0:03} not found under {1:?}")]
    Missing(u32, PathBuf),
    #[error("relative iter {0} past the start of the cycle (only {1} visit(s))")]
    OffStart(i32, usize),
    #[error("cycle directory {0:?} contains no visit-NNN entries")]
    Empty(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub fn list_visits(cycle_dir: &Path) -> Result<Vec<u32>, VisitError> {
    let mut out: Vec<u32> = Vec::new();
    for entry in std::fs::read_dir(cycle_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str()
            && let Some(rest) = name.strip_prefix("visit-")
            && let Ok(n) = rest.parse::<u32>()
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    Ok(out)
}

pub fn resolve_iter(cycle_dir: &Path, iter: Option<i32>) -> Result<u32, VisitError> {
    let visits = list_visits(cycle_dir)?;
    if visits.is_empty() {
        return Err(VisitError::Empty(cycle_dir.to_path_buf()));
    }
    match iter {
        None => Ok(*visits.last().unwrap()),
        Some(n) if n > 0 => {
            let n = n as u32;
            if visits.contains(&n) {
                Ok(n)
            } else {
                Err(VisitError::Missing(n, cycle_dir.to_path_buf()))
            }
        }
        Some(n) if n < 0 => {
            let back = (-n) as usize;
            if back > visits.len() {
                Err(VisitError::OffStart(n, visits.len()))
            } else {
                Ok(visits[visits.len() - back])
            }
        }
        Some(_) => Err(VisitError::Missing(0, cycle_dir.to_path_buf())),
    }
}

pub fn visit_dir(cycle_dir: &Path, n: u32) -> PathBuf {
    cycle_dir.join(format!("visit-{n:03}"))
}
```

- [ ] **Step 11: Run to verify pass**

```bash
cargo test -p roki-daemon --lib cli::shared::visit_lookup 2>&1 | tail -10
```

Expected: every test PASSes.

### 3.4 `tail`

- [ ] **Step 12: Write failing tests**

`crates/roki-daemon/src/cli/shared/tail.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    #[test]
    fn lines_returns_last_n_lines() {
        let f = fixture("a\nb\nc\nd\ne\n");
        let out = tail_lines(f.path(), 2).unwrap();
        assert_eq!(out, b"d\ne\n");
    }

    #[test]
    fn lines_returns_whole_file_when_fewer_than_n() {
        let f = fixture("a\nb\n");
        let out = tail_lines(f.path(), 10).unwrap();
        assert_eq!(out, b"a\nb\n");
    }

    #[test]
    fn lines_handles_missing_trailing_newline() {
        let f = fixture("x\ny\nz");
        let out = tail_lines(f.path(), 2).unwrap();
        assert_eq!(out, b"y\nz");
    }

    #[test]
    fn bytes_returns_suffix() {
        let f = fixture("abcdef");
        let out = tail_bytes(f.path(), 3).unwrap();
        assert_eq!(out, b"def");
    }

    #[test]
    fn bytes_returns_whole_file_when_shorter() {
        let f = fixture("xy");
        let out = tail_bytes(f.path(), 100).unwrap();
        assert_eq!(out, b"xy");
    }
}
```

- [ ] **Step 13: Run to verify failure**

```bash
cargo test -p roki-daemon --lib cli::shared::tail 2>&1 | tail -10
```

Expected: compile failure.

- [ ] **Step 14: Implement**

```rust
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub fn tail_bytes(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(n);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    if n == 0 || bytes.is_empty() {
        return Ok(Vec::new());
    }
    // Walk backwards through the byte slice counting newline boundaries.
    let mut newlines: usize = 0;
    let mut start: usize = 0;
    // Skip a final trailing newline so it doesn't count as a delimiter for line n.
    let last_is_nl = *bytes.last().unwrap() == b'\n';
    let scan_end = if last_is_nl { bytes.len() - 1 } else { bytes.len() };
    for i in (0..scan_end).rev() {
        if bytes[i] == b'\n' {
            newlines += 1;
            if newlines == n {
                start = i + 1;
                return Ok(bytes[start..].to_vec());
            }
        }
    }
    Ok(bytes)
}
```

- [ ] **Step 15: Run to verify pass**

```bash
cargo test -p roki-daemon --lib cli::shared::tail 2>&1 | tail -10
```

Expected: every test PASSes.

- [ ] **Step 16: Commit Task 3**

```bash
git add crates/roki-daemon/src/cli/shared/
git commit -m "$(cat <<'EOF'
feat(slice11): cli::shared (config_resolve, visit_lookup, tail)

config_resolve: --config / env fallback for session_root, API URL,
ticket/cycle, plus cross-ticket guard.

visit_lookup: list_visits, resolve_iter (absolute / negative /
latest), visit_dir path math against the cycle storage layout.

tail: tail_bytes and tail_lines for line- and byte-oriented suffix
reads.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `cli::shared::sanitize` + `cli::shared::events_format`

`format human` for `roki events` and ANSI strip for stdout-bound text. `api::sanitize::strip_ansi` is promoted to pub so we avoid a second VTE walker.

**Files:**
- Modify: `crates/roki-daemon/src/api/sanitize.rs` (promote `strip_ansi` to pub).
- Create: `crates/roki-daemon/src/cli/shared/sanitize.rs`, `crates/roki-daemon/src/cli/shared/events_format.rs`.
- Modify: `crates/roki-daemon/src/cli/shared/mod.rs` (re-export the two new modules).

### 4.1 Promote `strip_ansi`

- [ ] **Step 1: Make the existing fn public**

In `crates/roki-daemon/src/api/sanitize.rs`:

```rust
pub fn strip_ansi(input: &str) -> String {
    // body unchanged
}
```

Existing tests do not call it through `pub` so the only change is the visibility keyword.

- [ ] **Step 2: Verify the api crate still compiles**

```bash
cargo test -p roki-daemon --lib api::sanitize 2>&1 | tail -10
```

Expected: existing tests still PASS.

### 4.2 `cli::shared::sanitize`

- [ ] **Step 3: Create**

```rust
//! Terminal-output sanitization: ANSI strip + control-char strip.
//! Re-uses the VTE walker in [`crate::api::sanitize::strip_ansi`].

pub fn strip_for_terminal(input: &str) -> String {
    let ansi_stripped = crate::api::sanitize::strip_ansi(input);
    ansi_stripped
        .chars()
        .filter(|c| !is_disallowed_control(*c))
        .collect()
}

fn is_disallowed_control(c: char) -> bool {
    let cp = c as u32;
    // Keep tab, LF, CR; drop other C0 / DEL / C1 control codes.
    if matches!(c, '\t' | '\n' | '\r') {
        return false;
    }
    (cp < 0x20) || (0x7f..=0x9f).contains(&cp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_for_terminal("\x1b[31mok\x1b[0m"), "ok");
    }

    #[test]
    fn preserves_newline_tab_cr() {
        assert_eq!(strip_for_terminal("a\nb\tc\rd"), "a\nb\tc\rd");
    }

    #[test]
    fn drops_c0_controls() {
        assert_eq!(strip_for_terminal("\x07bell\x00null"), "bellnull");
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p roki-daemon --lib cli::shared::sanitize 2>&1 | tail -10
```

Expected: every test PASSes.

### 4.3 `cli::shared::events_format`

- [ ] **Step 5: Write the failing test**

`crates/roki-daemon/src/cli/shared/events_format.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use roki_api_types::ApiEvent;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn ev() -> ApiEvent {
        ApiEvent {
            seq: 42,
            ts: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            event: "webhook_received".into(),
            ticket_id: Some("ENG-1".into()),
            cycle_id: Some(
                Uuid::parse_str("12345678-1234-1234-1234-1234567890ab").unwrap(),
            ),
            payload: serde_json::json!({
                "title": "\x1b[31mhello\x1b[0m",
                "count": 3,
                "ok": true,
                "nested": {"a": 1}
            }),
        }
    }

    #[test]
    fn human_line_has_fixed_prefix_columns() {
        let line = format_human(&ev());
        assert!(line.starts_with("42  "));
        assert!(line.contains("webhook_received"));
        assert!(line.contains("ticket=ENG-1"));
        assert!(line.contains("cycle=12345678"));
    }

    #[test]
    fn human_line_strips_ansi_from_payload_strings() {
        let line = format_human(&ev());
        assert!(line.contains("title=hello"));
        assert!(!line.contains("\x1b"));
    }

    #[test]
    fn human_line_skips_object_and_array_payload_fields() {
        let line = format_human(&ev());
        assert!(!line.contains("nested"));
    }
}
```

- [ ] **Step 6: Run to verify failure**

```bash
cargo test -p roki-daemon --lib cli::shared::events_format 2>&1 | tail -10
```

Expected: compile failure (`format_human` undefined).

- [ ] **Step 7: Implement**

```rust
use roki_api_types::ApiEvent;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use crate::cli::shared::sanitize::strip_for_terminal;

pub fn format_human(ev: &ApiEvent) -> String {
    let ts = ev
        .ts
        .format(&Rfc3339)
        .unwrap_or_else(|_| "ts-format-error".into());
    let ticket = ev.ticket_id.as_deref().unwrap_or("-");
    let cycle = ev
        .cycle_id
        .map(|u| {
            let s = u.to_string();
            s.split('-').next().unwrap_or(&s).to_string()
        })
        .unwrap_or_else(|| "-".into());
    let mut line = format!(
        "{seq}  {ts}  {event}  ticket={ticket}  cycle={cycle}",
        seq = ev.seq,
        event = strip_for_terminal(&ev.event),
    );
    if let Value::Object(map) = &ev.payload {
        for (k, v) in map {
            match v {
                Value::String(s) => {
                    line.push_str(&format!("  {k}={}", strip_for_terminal(s)));
                }
                Value::Number(n) => line.push_str(&format!("  {k}={n}")),
                Value::Bool(b) => line.push_str(&format!("  {k}={b}")),
                _ => {}
            }
        }
    }
    line
}
```

- [ ] **Step 8: Run to verify pass**

```bash
cargo test -p roki-daemon --lib cli::shared::events_format 2>&1 | tail -10
```

Expected: every test PASSes.

### 4.4 Wire and commit

- [ ] **Step 9: Re-export**

In `crates/roki-daemon/src/cli/shared/mod.rs`:

```rust
pub mod config_resolve;
pub mod events_format;
pub mod sanitize;
pub mod tail;
pub mod visit_lookup;
```

- [ ] **Step 10: Commit**

```bash
git add crates/roki-daemon/src/api/sanitize.rs \
        crates/roki-daemon/src/cli/shared/
git commit -m "$(cat <<'EOF'
feat(slice11): cli::shared::sanitize + events_format

Promote api::sanitize::strip_ansi to pub so cli::shared::sanitize
reuses the existing VTE walker. cli::shared::sanitize wraps it with
a control-char strip suitable for terminal output. events_format
renders an ApiEvent into one human-readable line, scalar payload
fields only, ANSI-stripped.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `cli::repo` subcommand

Smallest of the three. Uses the already-built `engine::cwd::resolve`.

**Files:**
- Modify: `crates/roki-daemon/src/engine/cwd.rs` (remove `#![allow(dead_code)]`).
- Create: `crates/roki-daemon/src/cli/repo.rs`.
- Modify: `crates/roki-daemon/src/cli/mod.rs` (wire `pub mod repo;` + new `CliCommand::Repo` variant + dispatch).

- [ ] **Step 1: Write the failing dispatcher unit test**

`crates/roki-daemon/src/cli/repo.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn worktree_present_returns_worktree_path() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(wt_root.join("OPS-10")).unwrap();
        let ghq_base = tmp.path().join("ghq-base");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let out = temp_env::async_with_vars(
            [
                ("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap())),
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
            ],
            run_test(RepoArgs {
                ghq: Some("github.com/x/y".into()),
                ticket: Some("OPS-10".into()),
                worktree: false,
                auto_clone: false,
                config: None,
            }),
        )
        .await
        .unwrap();
        assert!(out.ends_with("OPS-10"), "got {out:?}");
    }

    #[tokio::test]
    async fn worktree_absent_returns_ghq_base() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(&wt_root).unwrap();
        let ghq_base = tmp.path().join("ghq-base");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let out = temp_env::async_with_vars(
            [
                ("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap())),
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
            ],
            run_test(RepoArgs {
                ghq: Some("github.com/x/y".into()),
                ticket: Some("OPS-10".into()),
                worktree: false,
                auto_clone: false,
                config: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(std::path::PathBuf::from(out), ghq_base);
    }

    #[tokio::test]
    async fn worktree_flag_strict_failure_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        std::fs::create_dir_all(&wt_root).unwrap();
        let ghq_base = tmp.path().join("ghq-base");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let err = temp_env::async_with_vars(
            [
                ("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap())),
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
            ],
            run_test(RepoArgs {
                ghq: Some("github.com/x/y".into()),
                ticket: Some("OPS-10".into()),
                worktree: true,
                auto_clone: false,
                config: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("worktree not yet materialized"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p roki-daemon --lib cli::repo 2>&1 | tail -10
```

Expected: compile failure (types undefined).

- [ ] **Step 3: Implement `RepoArgs`, `run`, `run_test`**

Above the `#[cfg(test)] mod tests`:

```rust
//! `roki repo` — resolve a ticket's worktree path (or ghq base fallback).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use thiserror::Error;

use crate::engine::{cwd, worktree};

#[derive(Debug, Args)]
pub struct RepoArgs {
    /// ghq slug (e.g., github.com/foo/bar). Defaults to $ROKI_REPO_GHQ.
    #[arg(value_name = "GHQ")]
    pub ghq: Option<String>,
    /// Ticket id. Defaults to $ROKI_TICKET_ID.
    #[arg(long = "ticket", value_name = "ID")]
    pub ticket: Option<String>,
    /// Require a materialized worktree; exit 1 otherwise.
    #[arg(long = "worktree")]
    pub worktree: bool,
    /// Run `ghq get <ghq>` before resolving the ghq base path.
    #[arg(long = "auto-clone")]
    pub auto_clone: bool,
    /// roki.toml path (optional).
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("roki repo: ghq slug required (pass <GHQ> or set $ROKI_REPO_GHQ)")]
    NoGhq,
    #[error("roki repo: ticket id required (pass --ticket or set $ROKI_TICKET_ID)")]
    NoTicket,
    #[error("roki repo: ghq get failed: {0}")]
    GhqGet(String),
    #[error("roki repo: worktree not yet materialized for ({ghq}, {ticket})")]
    NoWorktree { ghq: String, ticket: String },
    #[error("roki repo: {0}")]
    Resolve(String),
}

pub async fn run(args: RepoArgs) -> ExitCode {
    match run_inner(args).await {
        Ok(path) => {
            println!("{path}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            match err {
                RepoError::NoGhq | RepoError::NoTicket => ExitCode::from(2),
                _ => ExitCode::from(1),
            }
        }
    }
}

#[cfg(test)]
async fn run_test(args: RepoArgs) -> Result<String, RepoError> {
    run_inner(args).await
}

async fn run_inner(args: RepoArgs) -> Result<String, RepoError> {
    let ghq = args
        .ghq
        .or_else(|| std::env::var("ROKI_REPO_GHQ").ok().filter(|s| !s.is_empty()))
        .ok_or(RepoError::NoGhq)?;
    let ticket = args
        .ticket
        .or_else(|| std::env::var("ROKI_TICKET_ID").ok().filter(|s| !s.is_empty()))
        .ok_or(RepoError::NoTicket)?;

    if args.auto_clone {
        let out = tokio::process::Command::new("ghq")
            .arg("get")
            .arg(&ghq)
            .output()
            .await
            .map_err(|e| RepoError::GhqGet(format!("{e}")))?;
        if !out.status.success() {
            return Err(RepoError::GhqGet(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
    }

    if args.worktree {
        let wt = worktree::exists(&ghq, &ticket)
            .await
            .map_err(|e| RepoError::Resolve(format!("{e}")))?;
        match wt {
            Some(p) => Ok(p.to_string_lossy().into_owned()),
            None => Err(RepoError::NoWorktree { ghq, ticket }),
        }
    } else {
        let path = cwd::resolve(&ghq, &ticket)
            .await
            .map_err(|e| RepoError::Resolve(format!("{e}")))?;
        Ok(path.to_string_lossy().into_owned())
    }
}
```

- [ ] **Step 4: Wire into `cli/mod.rs`**

```rust
pub mod repo;

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    Run { #[arg(long = "config", value_name = "PATH")] config: PathBuf },
    Cleanup { #[arg(long = "config", value_name = "PATH")] config: PathBuf },
    Workflow { #[command(subcommand)] cmd: workflow::WorkflowCmd },
    /// Resolve a ticket's worktree path (or ghq base fallback).
    Repo(repo::RepoArgs),
}

// in run():
CliCommand::Repo(args) => repo::run(args).await,
```

- [ ] **Step 5: Drop `#![allow(dead_code)]` on `engine::cwd`**

`crates/roki-daemon/src/engine/cwd.rs:11` — delete the attribute line. The module is now used by `cli::repo`.

- [ ] **Step 6: Run the repo tests**

```bash
cargo test -p roki-daemon --lib cli::repo 2>&1 | tail -10
```

Expected: every test PASSes.

- [ ] **Step 7: Run the full crate tests**

```bash
cargo test -p roki-daemon --lib 2>&1 | tail -20
```

Expected: no regression.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/cli/mod.rs \
        crates/roki-daemon/src/cli/repo.rs \
        crates/roki-daemon/src/engine/cwd.rs
git commit -m "$(cat <<'EOF'
feat(slice11): roki repo subcommand

Thin wrapper over engine::cwd::resolve / worktree::exists. ghq slug
and ticket id default from $ROKI_REPO_GHQ / $ROKI_TICKET_ID env vars;
--worktree enforces a materialized worktree; --auto-clone runs
`ghq get <ghq>` before path resolution.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `cli::log` subcommand (without `--follow`)

All stream reads, list-visits, meta, tail/bytes. Follow lands in Task 7.

**Files:**
- Create: `crates/roki-daemon/src/cli/log.rs`.
- Modify: `crates/roki-daemon/src/cli/mod.rs` (wire `pub mod log;` + variant + dispatch).
- Modify: `crates/roki-daemon/Cargo.toml` (no new deps yet — uses what is already present).

- [ ] **Step 1: Write the failing test for `list-visits`**

`crates/roki-daemon/src/cli/log.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(session_root: &std::path::Path, ticket: &str, cycle: &str) -> std::path::PathBuf {
        let cycle_dir = session_root
            .join(ticket)
            .join(format!("cycle-{cycle}"));
        for n in 1..=2u32 {
            let vd = cycle_dir.join(format!("visit-{n:03}"));
            std::fs::create_dir_all(&vd).unwrap();
            std::fs::write(vd.join("impl.stdout"), format!("v{n} stdout\n")).unwrap();
            std::fs::write(vd.join("impl.stderr"), format!("v{n} stderr\n")).unwrap();
            std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();
        }
        cycle_dir
    }

    #[tokio::test]
    async fn list_visits_emits_jsonl_for_each_visit() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = fixture(tmp.path(), "ENG-1", "00000000-0000-0000-0000-000000000001");
        let env = [
            ("ROKI_CONFIG_SESSION_ROOT", Some(tmp.path().to_str().unwrap())),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            ("ROKI_CYCLE_ID", Some("00000000-0000-0000-0000-000000000001")),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                list_visits: true,
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"visit_n\":1"));
        assert!(lines[1].contains("\"visit_n\":2"));
        assert!(lines[0].contains("\"exit_code\":0"));
    }

    #[tokio::test]
    async fn stream_stdout_default_iter_is_latest() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = fixture(tmp.path(), "ENG-1", "00000000-0000-0000-0000-000000000001");
        let env = [
            ("ROKI_CONFIG_SESSION_ROOT", Some(tmp.path().to_str().unwrap())),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            ("ROKI_CYCLE_ID", Some("00000000-0000-0000-0000-000000000001")),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, "v2 stdout\n");
    }

    #[tokio::test]
    async fn stream_stdout_relative_iter_minus_one_reads_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = fixture(tmp.path(), "ENG-1", "00000000-0000-0000-0000-000000000001");
        let env = [
            ("ROKI_CONFIG_SESSION_ROOT", Some(tmp.path().to_str().unwrap())),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            ("ROKI_CYCLE_ID", Some("00000000-0000-0000-0000-000000000001")),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                iter: Some(-1),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        // -1 is the latest = visit-002 by convention in this plan; the spec
        // §4.2 step 3 defines "Relative -N → take dirs.len() - N (1-indexed)".
        assert_eq!(out, "v2 stdout\n");
    }

    #[tokio::test]
    async fn meta_emits_cycle_json_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let cycle_dir = fixture(tmp.path(), "ENG-1", "00000000-0000-0000-0000-000000000001");
        std::fs::write(cycle_dir.join("cycle.json"), r#"{"hello":"world"}"#).unwrap();
        let env = [
            ("ROKI_CONFIG_SESSION_ROOT", Some(tmp.path().to_str().unwrap())),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            ("ROKI_CYCLE_ID", Some("00000000-0000-0000-0000-000000000001")),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                meta: true,
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, r#"{"hello":"world"}"#);
    }

    #[tokio::test]
    async fn cross_ticket_refused_when_env_set_and_flag_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = fixture(tmp.path(), "ABC-1", "00000000-0000-0000-0000-000000000001");
        let env = [
            ("ROKI_CONFIG_SESSION_ROOT", Some(tmp.path().to_str().unwrap())),
            ("ROKI_TICKET_ID", Some("ABC-1")),
            ("ROKI_CYCLE_ID", Some("00000000-0000-0000-0000-000000000001")),
        ];
        let err = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                ticket: Some("XYZ-9".into()),
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("cross-ticket read refused"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p roki-daemon --lib cli::log 2>&1 | tail -10
```

Expected: compile failure.

- [ ] **Step 3: Implement**

```rust
//! `roki log` — read per-ticket subprocess captures.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use thiserror::Error;

use crate::cli::shared::{
    config_resolve::{enforce_same_ticket, resolve_session_root, resolve_ticket_and_cycle},
    tail::{tail_bytes, tail_lines},
    visit_lookup::{list_visits, resolve_iter, visit_dir},
};

#[derive(Debug, Default, Args)]
pub struct LogArgs {
    #[arg(long = "ticket", value_name = "ID")]
    pub ticket: Option<String>,
    #[arg(long = "cycle", value_name = "UUID")]
    pub cycle: Option<String>,
    #[arg(long = "state", value_name = "STATE_ID")]
    pub state: Option<String>,
    #[arg(long = "iter", value_name = "N", allow_negative_numbers = true)]
    pub iter: Option<i32>,
    #[arg(long = "stream", value_enum)]
    pub stream: Option<Stream>,
    #[arg(long = "tail", value_name = "N", conflicts_with = "bytes")]
    pub tail: Option<usize>,
    #[arg(long = "bytes", value_name = "N")]
    pub bytes: Option<u64>,
    #[arg(long = "list-visits")]
    pub list_visits: bool,
    #[arg(long = "meta")]
    pub meta: bool,
    #[arg(long = "follow")]
    pub follow: bool,
    #[arg(long = "follow-poll-ms", value_name = "MS", default_value_t = 200, hide = true)]
    pub follow_poll_ms: u64,
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Stream {
    Stdout,
    Stderr,
    Events,
    Terminal,
    Directive,
    ExitCode,
}

impl Stream {
    fn file_suffix(self) -> &'static str {
        match self {
            Stream::Stdout => ".stdout",
            Stream::Stderr => ".stderr",
            Stream::Events => ".events.jsonl",
            Stream::Terminal => ".terminal.json",
            Stream::Directive => ".directive.json",
            Stream::ExitCode => ".exit_code",
        }
    }
}

#[derive(Debug, Error)]
pub enum LogError {
    #[error("roki log: cannot resolve session_root (set --config or run from a state subprocess)")]
    NoSessionRoot,
    #[error("roki log: {0}")]
    Resolve(String),
    #[error("roki log: cross-ticket read refused")]
    CrossTicket,
    #[error("roki log: {0:?} not found")]
    NotFound(PathBuf),
    #[error("roki log: io: {0}")]
    Io(#[from] std::io::Error),
    #[error("roki log: {0}")]
    Other(String),
}

pub async fn run(args: LogArgs) -> ExitCode {
    match run_capture(args).await {
        Ok(bytes_or_text) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(bytes_or_text.as_bytes());
            ExitCode::SUCCESS
        }
        Err(LogError::CrossTicket) => {
            eprintln!("{}", LogError::CrossTicket);
            ExitCode::from(2)
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
pub(crate) async fn run_capture(args: LogArgs) -> Result<String, LogError> {
    let bytes = run_bytes(args).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn run_bytes(args: LogArgs) -> Result<Vec<u8>, LogError> {
    enforce_same_ticket(args.ticket.as_deref()).map_err(|_| LogError::CrossTicket)?;
    let session_root = resolve_session_root(args.config.as_deref())
        .map_err(|_| LogError::NoSessionRoot)?;
    let (ticket, cycle) = resolve_ticket_and_cycle(args.ticket.as_deref(), args.cycle.as_deref())
        .map_err(|e| LogError::Resolve(format!("{e}")))?;
    let cycle_dir = session_root.join(&ticket).join(format!("cycle-{cycle}"));

    if args.list_visits {
        return list_visits_jsonl(&cycle_dir);
    }
    if args.meta {
        let p = cycle_dir.join("cycle.json");
        return std::fs::read(&p).map_err(|_| LogError::NotFound(p));
    }
    // Stream read.
    let stream = args.stream.ok_or_else(|| {
        LogError::Other("roki log: --stream required (or pass --list-visits / --meta)".into())
    })?;
    let state = args.state.ok_or_else(|| {
        LogError::Other("roki log: --state required for stream reads".into())
    })?;
    let visit = resolve_iter(&cycle_dir, args.iter)
        .map_err(|e| LogError::Other(format!("{e}")))?;
    let file = visit_dir(&cycle_dir, visit).join(format!("{state}{}", stream.file_suffix()));
    if !file.exists() {
        return Err(LogError::NotFound(file));
    }
    if let Some(n) = args.tail {
        return tail_lines(&file, n).map_err(LogError::Io);
    }
    if let Some(n) = args.bytes {
        return tail_bytes(&file, n).map_err(LogError::Io);
    }
    std::fs::read(&file).map_err(|_| LogError::NotFound(file))
}

fn list_visits_jsonl(cycle_dir: &std::path::Path) -> Result<Vec<u8>, LogError> {
    let visits = list_visits(cycle_dir).map_err(|e| LogError::Other(format!("{e}")))?;
    let mut out = String::new();
    for n in visits {
        let vd = visit_dir(cycle_dir, n);
        let (state_id, exit_code) = pick_state_and_exit(&vd);
        out.push_str(&match exit_code {
            Some(code) => format!(
                "{{\"visit_n\":{n},\"state_id\":\"{state_id}\",\"exit_code\":{code}}}\n"
            ),
            None => format!("{{\"visit_n\":{n},\"state_id\":\"{state_id}\"}}\n"),
        });
    }
    Ok(out.into_bytes())
}

fn pick_state_and_exit(visit_dir: &std::path::Path) -> (String, Option<i32>) {
    let mut state_id = String::new();
    let mut exit_code: Option<i32> = None;
    if let Ok(read) = std::fs::read_dir(visit_dir) {
        for entry in read.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && let Some(rest) = name.strip_suffix(".exit_code")
            {
                state_id = rest.to_string();
                if let Ok(s) = std::fs::read_to_string(entry.path())
                    && let Ok(n) = s.trim().parse::<i32>()
                {
                    exit_code = Some(n);
                }
                break;
            }
        }
    }
    (state_id, exit_code)
}
```

- [ ] **Step 4: Wire into `cli/mod.rs`**

```rust
pub mod log;

pub enum CliCommand {
    // ... existing ...
    /// Read per-ticket subprocess captures (fr:09 §`roki log`).
    Log(log::LogArgs),
}

// in run():
CliCommand::Log(args) => log::run(args).await,
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p roki-daemon --lib cli::log 2>&1 | tail -10
```

Expected: every test PASSes.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/cli/log.rs crates/roki-daemon/src/cli/mod.rs
git commit -m "$(cat <<'EOF'
feat(slice11): roki log subcommand (no --follow yet)

Stream reads (stdout/stderr/events/terminal/directive/exit_code),
absolute and relative --iter, --tail N lines / --bytes N, --list-visits
JSON Lines, --meta cycle.json passthrough, cross-ticket guard.
--follow lands in Task 7.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `cli::log --follow`

Polling tail for `stdout` / `stderr` after EOF.

**Files:**
- Modify: `crates/roki-daemon/src/cli/log.rs` (wire `--follow` into `run_bytes` and add the polling loop).

- [ ] **Step 1: Write the failing test**

Append to `cli::log::tests`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_picks_up_late_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let cycle = "00000000-0000-0000-0000-000000000002";
    let cycle_dir = tmp.path().join("ENG-2").join(format!("cycle-{cycle}"));
    let vd = cycle_dir.join("visit-001");
    std::fs::create_dir_all(&vd).unwrap();
    let stdout_path = vd.join("impl.stdout");
    std::fs::write(&stdout_path, b"first\n").unwrap();

    // Writer task appends after 100 ms.
    let path_clone = stdout_path.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path_clone)
            .unwrap();
        f.write_all(b"second\n").unwrap();
        // signal end-of-test by writing the sentinel file the follower watches
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        std::fs::write(path_clone.with_extension("exit_code"), "0\n").unwrap();
    });

    let env = [
        ("ROKI_CONFIG_SESSION_ROOT", Some(tmp.path().to_str().unwrap())),
        ("ROKI_TICKET_ID", Some("ENG-2")),
        ("ROKI_CYCLE_ID", Some(cycle)),
    ];
    let collected = temp_env::async_with_vars(env, async {
        run_capture(LogArgs {
            state: Some("impl".into()),
            stream: Some(Stream::Stdout),
            follow: true,
            follow_poll_ms: 50,
            ..Default::default()
        })
        .await
    })
    .await
    .unwrap();
    writer.await.unwrap();
    assert!(collected.contains("first"));
    assert!(collected.contains("second"));
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p roki-daemon --lib cli::log::tests::follow_picks_up_late_writes 2>&1 | tail -10
```

Expected: FAIL (either the test hangs or `follow` is ignored). Cancel after a few seconds if it hangs and proceed.

- [ ] **Step 3: Implement the follow path**

In `run_bytes`, after the file-existence check but before `tail_lines` / `tail_bytes`, branch on `args.follow`:

```rust
if args.follow {
    if !matches!(stream, Stream::Stdout | Stream::Stderr) {
        return Err(LogError::Other(
            "roki log: --follow supported only with --stream stdout|stderr".into(),
        ));
    }
    return follow_file(&file, &cycle_dir, &state, args.follow_poll_ms).await;
}
```

Add `follow_file`:

```rust
async fn follow_file(
    file: &std::path::Path,
    cycle_dir: &std::path::Path,
    state: &str,
    poll_ms: u64,
) -> Result<Vec<u8>, LogError> {
    use tokio::io::AsyncReadExt;
    let mut collected = Vec::new();
    let mut f = tokio::fs::File::open(file).await.map_err(LogError::Io)?;
    let mut offset: u64 = 0;
    // Each iteration: read whatever is new, then sleep, then check the
    // exit-code sentinel that marks "writer is done".
    loop {
        let len = f.metadata().await.map_err(LogError::Io)?.len();
        if len > offset {
            let mut buf = vec![0u8; (len - offset) as usize];
            use tokio::io::AsyncSeekExt;
            f.seek(std::io::SeekFrom::Start(offset)).await.map_err(LogError::Io)?;
            f.read_exact(&mut buf).await.map_err(LogError::Io)?;
            // Mirror to stdout for non-test callers via `run`; the test
            // captures via `run_capture` so collect the bytes here.
            collected.extend_from_slice(&buf);
            offset = len;
        }
        // Termination signal: the daemon writes `<state>.exit_code` when
        // the visit finishes. Once present, drain any final bytes and exit.
        if cycle_dir
            .ancestors()
            .next()
            .is_some()
            && file
                .with_file_name(format!("{state}.exit_code"))
                .exists()
        {
            let len = f.metadata().await.map_err(LogError::Io)?.len();
            if len > offset {
                let mut buf = vec![0u8; (len - offset) as usize];
                use tokio::io::AsyncSeekExt;
                f.seek(std::io::SeekFrom::Start(offset)).await.map_err(LogError::Io)?;
                f.read_exact(&mut buf).await.map_err(LogError::Io)?;
                collected.extend_from_slice(&buf);
            }
            return Ok(collected);
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    }
}
```

For interactive use, `run` switches to a streaming path that writes bytes to stdout instead of accumulating; update `run` accordingly:

```rust
pub async fn run(args: LogArgs) -> ExitCode {
    if args.follow {
        match run_follow_streaming(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(1)
            }
        }
    } else {
        match run_capture_inner(args).await {
            Ok(bytes) => {
                use std::io::Write;
                let _ = std::io::stdout().write_all(&bytes);
                ExitCode::SUCCESS
            }
            Err(LogError::CrossTicket) => {
                eprintln!("{}", LogError::CrossTicket);
                ExitCode::from(2)
            }
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(1)
            }
        }
    }
}
```

`run_follow_streaming` mirrors `follow_file` but writes each chunk to stdout instead of into a `Vec`. Implementation:

```rust
async fn run_follow_streaming(args: LogArgs) -> Result<(), LogError> {
    // Reuse run_bytes path up to file resolution; for simplicity duplicate
    // the resolve block here so we can stream chunks directly.
    enforce_same_ticket(args.ticket.as_deref()).map_err(|_| LogError::CrossTicket)?;
    let session_root = resolve_session_root(args.config.as_deref())
        .map_err(|_| LogError::NoSessionRoot)?;
    let (ticket, cycle) = resolve_ticket_and_cycle(args.ticket.as_deref(), args.cycle.as_deref())
        .map_err(|e| LogError::Resolve(format!("{e}")))?;
    let cycle_dir = session_root.join(&ticket).join(format!("cycle-{cycle}"));
    let stream = args.stream.ok_or_else(|| {
        LogError::Other("roki log: --stream required for --follow".into())
    })?;
    if !matches!(stream, Stream::Stdout | Stream::Stderr) {
        return Err(LogError::Other(
            "roki log: --follow supported only with --stream stdout|stderr".into(),
        ));
    }
    let state = args.state.ok_or_else(|| {
        LogError::Other("roki log: --state required for --follow".into())
    })?;
    let visit = resolve_iter(&cycle_dir, args.iter)
        .map_err(|e| LogError::Other(format!("{e}")))?;
    let file = visit_dir(&cycle_dir, visit).join(format!("{state}{}", stream.file_suffix()));
    if !file.exists() {
        return Err(LogError::NotFound(file));
    }
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    let mut f = tokio::fs::File::open(&file).await.map_err(LogError::Io)?;
    let mut offset: u64 = 0;
    let exit_sentinel = file.with_file_name(format!("{state}.exit_code"));
    let mut stdout = tokio::io::stdout();
    loop {
        let len = f.metadata().await.map_err(LogError::Io)?.len();
        if len > offset {
            let mut buf = vec![0u8; (len - offset) as usize];
            f.seek(std::io::SeekFrom::Start(offset)).await.map_err(LogError::Io)?;
            f.read_exact(&mut buf).await.map_err(LogError::Io)?;
            stdout.write_all(&buf).await.map_err(LogError::Io)?;
            stdout.flush().await.map_err(LogError::Io)?;
            offset = len;
        }
        if exit_sentinel.exists() {
            // Drain anything written between the last poll and the sentinel write.
            let len = f.metadata().await.map_err(LogError::Io)?.len();
            if len > offset {
                let mut buf = vec![0u8; (len - offset) as usize];
                f.seek(std::io::SeekFrom::Start(offset)).await.map_err(LogError::Io)?;
                f.read_exact(&mut buf).await.map_err(LogError::Io)?;
                stdout.write_all(&buf).await.map_err(LogError::Io)?;
                stdout.flush().await.map_err(LogError::Io)?;
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(args.follow_poll_ms)).await;
    }
}
```

Refactor: `run_bytes` becomes `run_capture_inner` for tests (returns bytes), with the public test helper `run_capture` calling either the streaming follower (collecting to a String) when `args.follow` is true, or `run_capture_inner` otherwise. Adjust:

```rust
#[cfg(test)]
pub(crate) async fn run_capture(args: LogArgs) -> Result<String, LogError> {
    if args.follow {
        let bytes = follow_file_for_test(args).await?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let bytes = run_capture_inner(args).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
async fn follow_file_for_test(args: LogArgs) -> Result<Vec<u8>, LogError> {
    enforce_same_ticket(args.ticket.as_deref()).map_err(|_| LogError::CrossTicket)?;
    let session_root = resolve_session_root(args.config.as_deref())
        .map_err(|_| LogError::NoSessionRoot)?;
    let (ticket, cycle) = resolve_ticket_and_cycle(args.ticket.as_deref(), args.cycle.as_deref())
        .map_err(|e| LogError::Resolve(format!("{e}")))?;
    let cycle_dir = session_root.join(&ticket).join(format!("cycle-{cycle}"));
    let stream = args.stream.ok_or_else(|| {
        LogError::Other("roki log: --stream required for --follow".into())
    })?;
    let state = args.state.ok_or_else(|| {
        LogError::Other("roki log: --state required for --follow".into())
    })?;
    let visit = resolve_iter(&cycle_dir, args.iter)
        .map_err(|e| LogError::Other(format!("{e}")))?;
    let file = visit_dir(&cycle_dir, visit).join(format!("{state}{}", stream.file_suffix()));
    if !file.exists() {
        return Err(LogError::NotFound(file));
    }
    follow_file(&file, &cycle_dir, &state, args.follow_poll_ms).await
}
```

Rename the original `run_bytes` to `run_capture_inner`.

- [ ] **Step 4: Run the follow test**

```bash
cargo test -p roki-daemon --lib cli::log::tests::follow_picks_up_late_writes 2>&1 | tail -10
```

Expected: PASS within ~500 ms.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/cli/log.rs
git commit -m "$(cat <<'EOF'
feat(slice11): roki log --follow

Polling tail for stdout/stderr with a configurable cadence (hidden
--follow-poll-ms for tests). Terminates when the visit's
<state>.exit_code sentinel appears, after draining any final bytes.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `cli::events` — offline reader

Lands before the online path because it has no async / network surface.

**Files:**
- Create: `crates/roki-daemon/src/cli/events.rs`.
- Modify: `crates/roki-daemon/src/cli/mod.rs` (wire `pub mod events;` + variant + dispatch).

- [ ] **Step 1: Write the failing tests**

`crates/roki-daemon/src/cli/events.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &std::path::Path) {
        std::fs::write(
            path,
            concat!(
                r#"{"seq":1,"ts":"2026-05-11T10:00:00Z","event":"webhook_received","ticket_id":"ENG-1","cycle_id":null,"payload":{"foo":"bar"}}"#,
                "\n",
                r#"{"seq":2,"ts":"2026-05-11T10:00:01Z","event":"cycle_started","ticket_id":"ENG-1","cycle_id":"00000000-0000-0000-0000-000000000001","payload":{"kind":"rule"}}"#,
                "\n",
                r#"{"seq":3,"ts":"2026-05-11T10:00:02Z","event":"state_started","ticket_id":"ENG-2","cycle_id":null,"payload":{}}"#,
                "\n",
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn offline_filter_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.jsonl");
        fixture(&path);
        let out = run_capture(EventsArgs {
            offline: true,
            file: Some(path),
            kind: Some("cycle_started".into()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"event\":\"cycle_started\""));
    }

    #[tokio::test]
    async fn offline_filter_ticket() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.jsonl");
        fixture(&path);
        let out = run_capture(EventsArgs {
            offline: true,
            file: Some(path),
            ticket: Some("ENG-2".into()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(out.lines().count(), 1);
    }

    #[tokio::test]
    async fn offline_human_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.jsonl");
        fixture(&path);
        let out = run_capture(EventsArgs {
            offline: true,
            file: Some(path),
            format: Format::Human,
            ..Default::default()
        })
        .await
        .unwrap();
        let first = out.lines().next().unwrap();
        assert!(first.starts_with("1  "));
        assert!(first.contains("ticket=ENG-1"));
    }

    #[tokio::test]
    async fn offline_since_rfc3339_drops_strictly_older() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.jsonl");
        fixture(&path);
        let out = run_capture(EventsArgs {
            offline: true,
            file: Some(path),
            since: Some("2026-05-11T10:00:01Z".into()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // ts==target is kept; ts<target is dropped.
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"seq\":2"));
        assert!(lines[1].contains("\"seq\":3"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p roki-daemon --lib cli::events 2>&1 | tail -10
```

Expected: compile failure.

- [ ] **Step 3: Implement**

```rust
//! `roki events` — read the structured event stream (online HTTP or offline file).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use roki_api_types::ApiEvent;
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::cli::shared::events_format::format_human;

#[derive(Debug, Default, Args)]
pub struct EventsArgs {
    #[arg(long = "tail")]
    pub tail: bool,
    #[arg(long = "since", value_name = "SEQ_OR_RFC3339")]
    pub since: Option<String>,
    #[arg(long = "kind", value_name = "EVENT")]
    pub kind: Option<String>,
    #[arg(long = "ticket", value_name = "ID")]
    pub ticket: Option<String>,
    #[arg(long = "cycle", value_name = "UUID")]
    pub cycle: Option<String>,
    #[arg(long = "format", value_enum, default_value_t = Format::Json)]
    pub format: Format,
    #[arg(long = "api", value_name = "URL")]
    pub api: Option<String>,
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long = "offline")]
    pub offline: bool,
    #[arg(long = "file", value_name = "PATH")]
    pub file: Option<PathBuf>,
    #[arg(long = "cadence-ms", value_name = "MS", default_value_t = 1000, hide = true)]
    pub cadence_ms: u64,
}

#[derive(Debug, Default, Clone, Copy, ValueEnum)]
pub enum Format {
    #[default]
    Json,
    Human,
}

#[derive(Debug, Error)]
pub enum EventsError {
    #[error("roki events: {0}")]
    Resolve(String),
    #[error("roki events: --offline requires --file")]
    NoFile,
    #[error("roki events: --tail not supported with --offline")]
    OfflineTail,
    #[error("roki events: io: {0}")]
    Io(#[from] std::io::Error),
    #[error("roki events: http: {0}")]
    Http(String),
    #[error("roki events: bad event line: {0}")]
    BadLine(String),
}

pub async fn run(args: EventsArgs) -> ExitCode {
    let format = args.format;
    if args.offline {
        if args.tail {
            eprintln!("{}", EventsError::OfflineTail);
            return ExitCode::from(2);
        }
        return run_offline_dispatch(args, format).await;
    }
    run_online_dispatch(args, format).await
}

async fn run_offline_dispatch(args: EventsArgs, format: Format) -> ExitCode {
    match run_capture(args).await {
        Ok(text) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(text.as_bytes());
            let _ = std::io::stdout().write_all(b"\n");
            let _ = format;
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
    }
}

async fn run_online_dispatch(_args: EventsArgs, _format: Format) -> ExitCode {
    // Wired in Task 9.
    eprintln!("roki events: online mode not yet implemented (slice 11 Task 9)");
    ExitCode::from(70)
}

#[cfg(test)]
pub(crate) async fn run_capture(args: EventsArgs) -> Result<String, EventsError> {
    if args.offline {
        return run_offline_capture(args).await;
    }
    Err(EventsError::Resolve("test path is offline-only".into()))
}

async fn run_offline_capture(args: EventsArgs) -> Result<String, EventsError> {
    let file = args.file.ok_or(EventsError::NoFile)?;
    let raw = std::fs::read_to_string(&file)?;
    let filter = Filter::from_args(&args)?;
    let mut out = String::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: ApiEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => {
                eprintln!("# roki events: skipping malformed line");
                continue;
            }
        };
        if !filter.accept(&ev) {
            continue;
        }
        match args.format {
            Format::Json => {
                out.push_str(line);
                out.push('\n');
            }
            Format::Human => {
                out.push_str(&format_human(&ev));
                out.push('\n');
            }
        }
    }
    // Trim trailing newline so tests can `out.lines()` exactly.
    if out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

struct Filter {
    since_seq: Option<u64>,
    since_ts: Option<OffsetDateTime>,
    kind: Option<String>,
    ticket: Option<String>,
    cycle: Option<Uuid>,
}

impl Filter {
    fn from_args(args: &EventsArgs) -> Result<Self, EventsError> {
        let (since_seq, since_ts) = match args.since.as_deref() {
            None => (None, None),
            Some(s) => {
                if let Ok(n) = s.parse::<u64>() {
                    (Some(n), None)
                } else {
                    let ts = OffsetDateTime::parse(s, &Rfc3339).map_err(|_| {
                        EventsError::Resolve(format!("invalid --since value: {s}"))
                    })?;
                    (None, Some(ts))
                }
            }
        };
        let cycle = args
            .cycle
            .as_deref()
            .map(|s| Uuid::parse_str(s).map_err(|e| EventsError::Resolve(format!("{e}"))))
            .transpose()?;
        Ok(Self {
            since_seq,
            since_ts,
            kind: args.kind.clone(),
            ticket: args.ticket.clone(),
            cycle,
        })
    }

    fn accept(&self, ev: &ApiEvent) -> bool {
        if let Some(seq) = self.since_seq
            && ev.seq < seq
        {
            return false;
        }
        if let Some(ts) = self.since_ts
            && ev.ts < ts
        {
            return false;
        }
        if let Some(k) = &self.kind
            && ev.event != *k
        {
            return false;
        }
        if let Some(t) = &self.ticket
            && ev.ticket_id.as_deref() != Some(t.as_str())
        {
            return false;
        }
        if let Some(c) = self.cycle
            && ev.cycle_id != Some(c)
        {
            return false;
        }
        true
    }
}
```

- [ ] **Step 4: Wire into `cli/mod.rs`**

```rust
pub mod events;

pub enum CliCommand {
    // ... existing ...
    /// Read the structured event stream (live HTTP or offline file).
    Events(events::EventsArgs),
}

// in run():
CliCommand::Events(args) => events::run(args).await,
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p roki-daemon --lib cli::events 2>&1 | tail -10
```

Expected: every offline test PASSes.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/cli/events.rs crates/roki-daemon/src/cli/mod.rs
git commit -m "$(cat <<'EOF'
feat(slice11): roki events --offline reader

Reads a JSON Lines file produced by tracing's file destination,
applies --kind / --ticket / --cycle / --since filters (seq or
rfc3339; rfc3339 cutoff is client-side), emits JSON or human format.
Online mode lands in Task 9.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: `cli::events` — online HTTP client

Polling loop against `/api/events?since=<seq>`.

**Files:**
- Modify: `crates/roki-daemon/Cargo.toml` (add `reqwest` to `[dependencies]`, `wiremock` to `[dev-dependencies]`).
- Modify: `crates/roki-daemon/src/cli/events.rs` (implement `run_online_dispatch` and remove the stub).

- [ ] **Step 1: Add `reqwest` and `wiremock`**

In `crates/roki-daemon/Cargo.toml`:

```toml
[dependencies]
# ... existing ...
reqwest = { workspace = true, features = ["json", "rustls-tls"] }

[dev-dependencies]
# ... existing ...
wiremock = "0.6"
```

If `reqwest` is not in `[workspace.dependencies]`, mirror the version `roki-tui` uses in the workspace `Cargo.toml`.

- [ ] **Step 2: Write the failing online test**

Append to `cli::events::tests`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn online_dump_returns_events_from_api() {
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use wiremock::matchers::{method, path, query_param};

    let server = MockServer::start().await;
    let body = serde_json::json!({
        "events": [
            {
                "seq": 1,
                "ts": "2026-05-11T10:00:00Z",
                "event": "webhook_received",
                "ticket_id": "ENG-1",
                "payload": {"k": "v"}
            }
        ],
        "gap": false,
        "next_since": 2,
    });
    Mock::given(method("GET"))
        .and(path("/api/events"))
        .and(query_param("since", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let out = run_capture_online(EventsArgs {
        api: Some(server.uri()),
        format: Format::Json,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(out.contains("\"seq\":1"));
    assert!(out.contains("webhook_received"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn online_resolve_api_url_errors_with_no_source() {
    let err = temp_env::async_with_vars([("ROKI_API_URL", None::<&str>)], async {
        run_capture_online(EventsArgs {
            api: None,
            config: None,
            ..Default::default()
        })
        .await
        .unwrap_err()
    })
    .await;
    assert!(format!("{err}").contains("cannot resolve API URL"));
}
```

- [ ] **Step 3: Run to verify failure**

```bash
cargo test -p roki-daemon --lib cli::events::tests::online_ 2>&1 | tail -10
```

Expected: compile failure (`run_capture_online` undefined).

- [ ] **Step 4: Implement online path**

In `cli::events`, replace `run_online_dispatch` and `run_capture`'s online branch:

```rust
use crate::cli::shared::config_resolve::resolve_api_url;

pub async fn run(args: EventsArgs) -> ExitCode {
    let format = args.format;
    if args.offline {
        if args.tail {
            eprintln!("{}", EventsError::OfflineTail);
            return ExitCode::from(2);
        }
        return run_offline_dispatch(args, format).await;
    }
    run_online_dispatch(args, format).await
}

async fn run_online_dispatch(args: EventsArgs, _format: Format) -> ExitCode {
    let base = match resolve_api_url(args.api.as_deref(), args.config.as_deref()) {
        Ok(u) => u,
        Err(err) => {
            eprintln!("roki events: {err}");
            return ExitCode::from(1);
        }
    };
    match run_online(args, base, &mut StdoutSink).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
pub(crate) async fn run_capture_online(args: EventsArgs) -> Result<String, EventsError> {
    let base = resolve_api_url(args.api.as_deref(), args.config.as_deref())
        .map_err(|e| EventsError::Resolve(format!("{e}")))?;
    let mut sink = StringSink(String::new());
    run_online(args, base, &mut sink).await?;
    Ok(sink.0)
}

trait Sink {
    fn write_line(&mut self, line: &str) -> Result<(), EventsError>;
}

struct StdoutSink;
impl Sink for StdoutSink {
    fn write_line(&mut self, line: &str) -> Result<(), EventsError> {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        out.write_all(line.as_bytes()).map_err(EventsError::Io)?;
        out.write_all(b"\n").map_err(EventsError::Io)
    }
}

#[cfg(test)]
struct StringSink(String);
#[cfg(test)]
impl Sink for StringSink {
    fn write_line(&mut self, line: &str) -> Result<(), EventsError> {
        self.0.push_str(line);
        self.0.push('\n');
        Ok(())
    }
}

async fn run_online(args: EventsArgs, base: String, sink: &mut dyn Sink) -> Result<(), EventsError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| EventsError::Http(format!("{e}")))?;
    let filter = Filter::from_args(&args)?;
    let mut since: u64 = filter.since_seq.unwrap_or(0);
    let ticket = args.ticket.clone();
    let cycle = args.cycle.clone();
    let kind = args.kind.clone();
    let format = args.format;
    let cadence = std::time::Duration::from_millis(args.cadence_ms);

    loop {
        let url = format!("{base}/api/events");
        let mut req = client.get(&url).query(&[("since", since.to_string())]);
        if let Some(k) = &kind {
            req = req.query(&[("kind", k.clone())]);
        }
        if let Some(t) = &ticket {
            req = req.query(&[("ticket", t.clone())]);
        }
        if let Some(c) = &cycle {
            req = req.query(&[("cycle", c.clone())]);
        }
        let page: roki_api_types::EventsPage = match req.send().await {
            Ok(r) => match r.error_for_status() {
                Ok(r) => r.json().await.map_err(|e| EventsError::Http(format!("{e}")))?,
                Err(e) => return Err(EventsError::Http(format!("{e}"))),
            },
            Err(e) => return Err(EventsError::Http(format!("{e}"))),
        };
        if page.gap {
            eprintln!("# roki events: ring gap detected; consult [log].file_path");
        }
        for ev in &page.events {
            if !filter.accept_after_seq_cursor(ev) {
                continue;
            }
            match format {
                Format::Json => {
                    let line = serde_json::to_string(ev)
                        .map_err(|e| EventsError::Http(format!("{e}")))?;
                    sink.write_line(&line)?;
                }
                Format::Human => sink.write_line(&format_human(ev))?,
            }
        }
        match page.next_since {
            Some(n) => since = n,
            None => break,
        }
        if !args.tail {
            break;
        }
        tokio::time::sleep(cadence).await;
    }
    Ok(())
}
```

Add a small helper on `Filter`:

```rust
impl Filter {
    // Server-side seq cursor is already applied via the `since` query
    // param. For RFC3339 cutoffs the client must drop strictly-older
    // events. Other filters are server-applied in online mode, so we
    // skip them here.
    fn accept_after_seq_cursor(&self, ev: &ApiEvent) -> bool {
        if let Some(ts) = self.since_ts
            && ev.ts < ts
        {
            return false;
        }
        true
    }
}
```

- [ ] **Step 5: Run the online tests**

```bash
cargo test -p roki-daemon --lib cli::events::tests::online_ 2>&1 | tail -20
```

Expected: both PASS.

- [ ] **Step 6: Smoke the whole `cli::events` suite**

```bash
cargo test -p roki-daemon --lib cli::events 2>&1 | tail -20
```

Expected: no regression on the offline tests; new online tests PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/src/cli/events.rs
git commit -m "$(cat <<'EOF'
feat(slice11): roki events online HTTP client

Polling client against /api/events?since=<seq>. Single-shot when
--tail unset; loops at --cadence-ms with the API's next_since cursor
otherwise. Surfaces gap=true to stderr and continues. Resolves API
URL via --api / $ROKI_API_URL / --config [api] (in that order).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: e2e integration tests

End-to-end coverage that drives the actual `roki` binary against fixtures.

**Files:**
- Modify: `crates/roki-daemon/Cargo.toml` (add `[[test]]` entries).
- Create: `crates/roki-daemon/tests/e2e/cli_log_smoke.rs`, `cli_log_follow.rs`, `cli_events_online_smoke.rs`, `cli_events_offline_smoke.rs`, `cli_repo_smoke.rs`.

- [ ] **Step 1: Add `[[test]]` entries**

In `crates/roki-daemon/Cargo.toml`:

```toml
[[test]]
name = "cli_log_smoke"
path = "tests/e2e/cli_log_smoke.rs"

[[test]]
name = "cli_log_follow"
path = "tests/e2e/cli_log_follow.rs"

[[test]]
name = "cli_events_online_smoke"
path = "tests/e2e/cli_events_online_smoke.rs"

[[test]]
name = "cli_events_offline_smoke"
path = "tests/e2e/cli_events_offline_smoke.rs"

[[test]]
name = "cli_repo_smoke"
path = "tests/e2e/cli_repo_smoke.rs"
```

- [ ] **Step 2: Implement `cli_log_smoke.rs`**

`crates/roki-daemon/tests/e2e/cli_log_smoke.rs`:

```rust
//! E2E: drive `roki log` against a synthetic visit directory.
use std::process::Command;

fn bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // /target/debug/deps
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("roki")
}

#[test]
fn stream_stdout_reads_visit_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let cycle = "00000000-0000-0000-0000-00000000000a";
    let vd = tmp
        .path()
        .join("ENG-9")
        .join(format!("cycle-{cycle}"))
        .join("visit-001");
    std::fs::create_dir_all(&vd).unwrap();
    std::fs::write(vd.join("impl.stdout"), b"hello\nworld\n").unwrap();
    std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();

    let out = Command::new(bin())
        .env("ROKI_CONFIG_SESSION_ROOT", tmp.path())
        .env("ROKI_TICKET_ID", "ENG-9")
        .env("ROKI_CYCLE_ID", cycle)
        .args(["log", "--state", "impl", "--stream", "stdout"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout, b"hello\nworld\n");
}

#[test]
fn list_visits_emits_per_visit_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    let cycle = "00000000-0000-0000-0000-00000000000b";
    let cycle_dir = tmp.path().join("ENG-9").join(format!("cycle-{cycle}"));
    for n in 1..=3u32 {
        let vd = cycle_dir.join(format!("visit-{n:03}"));
        std::fs::create_dir_all(&vd).unwrap();
        std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();
    }
    let out = Command::new(bin())
        .env("ROKI_CONFIG_SESSION_ROOT", tmp.path())
        .env("ROKI_TICKET_ID", "ENG-9")
        .env("ROKI_CYCLE_ID", cycle)
        .args(["log", "--list-visits"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    assert_eq!(s.lines().count(), 3);
}
```

- [ ] **Step 3: Implement `cli_log_follow.rs`**

```rust
//! E2E: `roki log --follow` picks up late writes.
use std::process::{Command, Stdio};
use std::time::Duration;

fn bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("roki")
}

#[test]
fn follow_streams_late_appends_then_exits_on_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let cycle = "00000000-0000-0000-0000-00000000000c";
    let vd = tmp
        .path()
        .join("ENG-9")
        .join(format!("cycle-{cycle}"))
        .join("visit-001");
    std::fs::create_dir_all(&vd).unwrap();
    let stdout = vd.join("impl.stdout");
    std::fs::write(&stdout, b"first\n").unwrap();

    let mut child = Command::new(bin())
        .env("ROKI_CONFIG_SESSION_ROOT", tmp.path())
        .env("ROKI_TICKET_ID", "ENG-9")
        .env("ROKI_CYCLE_ID", cycle)
        .args(["log", "--state", "impl", "--stream", "stdout", "--follow", "--follow-poll-ms", "50"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(100));
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&stdout).unwrap();
    f.write_all(b"second\n").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();

    let waited = child.wait_with_output().unwrap();
    assert!(waited.status.success());
    let s = String::from_utf8(waited.stdout).unwrap();
    assert!(s.contains("first"));
    assert!(s.contains("second"));
}
```

- [ ] **Step 4: Implement `cli_events_offline_smoke.rs`**

```rust
//! E2E: `roki events --offline --file <p>` JSON Lines reader.
use std::process::Command;

fn bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("roki")
}

#[test]
fn offline_kind_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("daemon.jsonl");
    std::fs::write(
        &file,
        concat!(
            r#"{"seq":1,"ts":"2026-05-11T10:00:00Z","event":"webhook_received","ticket_id":"ENG-1","payload":{}}"#,
            "\n",
            r#"{"seq":2,"ts":"2026-05-11T10:00:01Z","event":"cycle_started","ticket_id":"ENG-1","payload":{}}"#,
            "\n",
        ),
    )
    .unwrap();
    let out = Command::new(bin())
        .args([
            "events",
            "--offline",
            "--file",
            file.to_str().unwrap(),
            "--kind",
            "cycle_started",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("cycle_started"));
    assert!(!s.contains("webhook_received"));
}
```

- [ ] **Step 5: Implement `cli_events_online_smoke.rs`**

```rust
//! E2E: `roki events --tail` against a wiremock server.
use std::process::Command;

fn bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("roki")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn online_dump_against_wiremock() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let body = serde_json::json!({
        "events": [{
            "seq": 1,
            "ts": "2026-05-11T10:00:00Z",
            "event": "webhook_received",
            "ticket_id": "ENG-3",
            "payload": {"foo": "bar"}
        }],
        "gap": false,
        "next_since": 2,
    });
    Mock::given(method("GET"))
        .and(path("/api/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let out = Command::new(bin())
        .args(["events", "--api", &server.uri()])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("\"seq\":1"));
    assert!(s.contains("webhook_received"));
}
```

- [ ] **Step 6: Implement `cli_repo_smoke.rs`**

```rust
//! E2E: `roki repo` against overridden ghq base + worktree root.
use std::process::Command;

fn bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("roki")
}

#[test]
fn repo_returns_ghq_base_when_no_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let wt_root = tmp.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();
    let ghq_base = tmp.path().join("ghq-base");
    std::fs::create_dir_all(&ghq_base).unwrap();
    let out = Command::new(bin())
        .env("ROKI_GHQ_BASE_OVERRIDE", &ghq_base)
        .env("ROKI_WT_ROOT_OVERRIDE", &wt_root)
        .args(["repo", "github.com/x/y", "--ticket", "OPS-11"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8(out.stdout).unwrap();
    assert_eq!(s.trim(), ghq_base.to_string_lossy());
}

#[test]
fn repo_worktree_flag_errors_when_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let wt_root = tmp.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();
    let ghq_base = tmp.path().join("ghq-base");
    std::fs::create_dir_all(&ghq_base).unwrap();
    let out = Command::new(bin())
        .env("ROKI_GHQ_BASE_OVERRIDE", &ghq_base)
        .env("ROKI_WT_ROOT_OVERRIDE", &wt_root)
        .args(["repo", "github.com/x/y", "--ticket", "OPS-11", "--worktree"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let s = String::from_utf8(out.stderr).unwrap();
    assert!(s.contains("worktree not yet materialized"));
}
```

- [ ] **Step 7: Build and run every new e2e**

```bash
cargo test -p roki-daemon --test cli_log_smoke 2>&1 | tail -10
cargo test -p roki-daemon --test cli_log_follow 2>&1 | tail -10
cargo test -p roki-daemon --test cli_events_offline_smoke 2>&1 | tail -10
cargo test -p roki-daemon --test cli_events_online_smoke 2>&1 | tail -10
cargo test -p roki-daemon --test cli_repo_smoke 2>&1 | tail -10
```

Expected: each suite PASSes. If `bin()` cannot locate the freshly-built `roki` binary, switch to using `env!("CARGO_BIN_EXE_roki")` instead (Cargo exposes this for tests in the same package as the binary).

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/tests/e2e/cli_*.rs
git commit -m "$(cat <<'EOF'
test(slice11): e2e coverage for roki log/events/repo

Drive the freshly built roki binary against synthetic fixtures and a
wiremock HTTP server. Five suites:

- cli_log_smoke (stream read, list-visits)
- cli_log_follow (--follow late writes + exit_code sentinel)
- cli_events_offline_smoke (kind filter, JSON output)
- cli_events_online_smoke (--tail dump against wiremock)
- cli_repo_smoke (worktree absent → ghq base; --worktree strict failure)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: FR 09 documentation patches + `ref:cli` update

**Files:**
- Modify: `docs/fr/09-log-access-cli.md`.
- Create or modify: `docs/reference/cli.md` (kusara `ref:cli` doc).

- [ ] **Step 1: Patch fr:09**

In `docs/fr/09-log-access-cli.md`:

- Replace `--repo $ROKI_REPO` → `--repo $ROKI_REPO_GHQ` (in the `roki repo` examples block).
- Replace every `meta.json` mention with `cycle.json` (the storage-layout snippet under §`roki log` → Storage layout, and the `--meta` description).
- After the `--cycle <uuid>` example under `roki events`, add a sentence: "The HTTP API URL is resolved in this order: `--api <URL>` flag, `$ROKI_API_URL` env, `[api]` section of `--config <roki.toml>`. If none resolve, `roki events` exits 1."
- After the `roki log --cycle <uuid>` example, add: "Inside a state subprocess, `roki log` reads `paths.session_root` from `$ROKI_CONFIG_SESSION_ROOT`. External callers pass `--config <PATH>`."

Run the kusara post-edit hook by re-saving the file; then:

```bash
kusara validate
```

Expected: `OK` (no new dangling refs).

- [ ] **Step 2: Create or extend `docs/reference/cli.md`**

If the file does not exist yet, create it with the kusara frontmatter pattern:

```markdown
---
refs:
  id: ref:cli
  kind: reference
  title: "CLI surface"
  spec: roki-cli
  related:
    - fr:09-log-access-cli
    - fr:10-http-api
---

# CLI surface

The `roki` binary exposes the following subcommands. The flag tables here
are the single source of truth; FR 09 narrates the operator-facing model
and FR 10 covers the HTTP API that `roki events` consumes.

## `roki log`

| Flag | Notes |
|---|---|
| ... copy the table from the slice 11 spec §4.1 ... |

## `roki events`

| Flag | Notes |
|---|---|
| ... copy from §5.1 ... |

## `roki repo`

| Flag | Notes |
|---|---|
| ... copy from §6.1 ... |
```

If the file already exists, append the three sections to the end. Tighten the kind glob in `docs/kinds.md` only if validate complains (it should not — `reference` kind already matches `docs/reference/[a-z]*.md`).

- [ ] **Step 3: Validate the graph**

```bash
kusara validate
```

Expected: `OK`.

- [ ] **Step 4: Commit**

```bash
git add docs/fr/09-log-access-cli.md docs/reference/cli.md
git commit -m "$(cat <<'EOF'
docs(slice11): fr:09 fixes + ref:cli CLI surface

fr:09: align $ROKI_REPO_GHQ and cycle.json with runtime reality;
document $ROKI_API_URL / --api and $ROKI_CONFIG_SESSION_ROOT /
--config defaults.

ref:cli: canonical flag tables for `roki log`, `roki events`,
`roki repo`.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Final sweep

- [ ] **Step 1: Format**

```bash
cargo fmt --all
```

- [ ] **Step 2: Clippy clean**

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: no warnings.

- [ ] **Step 3: Full workspace tests**

```bash
cargo test --workspace 2>&1 | tail -20
```

Expected: every suite green.

- [ ] **Step 4: Help-text smoke**

```bash
cargo run -q -p roki-daemon -- --help 2>&1 | head -30
cargo run -q -p roki-daemon -- log --help 2>&1 | head -40
cargo run -q -p roki-daemon -- events --help 2>&1 | head -40
cargo run -q -p roki-daemon -- repo --help 2>&1 | head -20
```

Expected: each `--help` listing matches the flag tables documented in `docs/reference/cli.md`. If a column drifted, update either the doc or the clap attributes so they match.

- [ ] **Step 5: Validate docs graph one last time**

```bash
kusara validate
```

Expected: `OK`.

- [ ] **Step 6: Commit any cleanup**

If formatting or clippy produced edits:

```bash
git add -u
git commit -m "$(cat <<'EOF'
chore(slice11): fmt + clippy sweep

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

Otherwise skip this step.

---

## Self-Review Checklist (run after writing — informational)

- **Spec coverage:**
  - §3 daemon env hooks → Task 1.
  - §4 `roki log` (no follow) → Task 6.
  - §4 `roki log --follow` → Task 7.
  - §5 `roki events` offline → Task 8.
  - §5 `roki events` online + `--tail` + `--format human` → Task 9 (with `format_human` in Task 4).
  - §6 `roki repo` → Task 5.
  - §7 Sanitization → Task 4.
  - §8 Config touch points → none new (consumed in Tasks 6 and 9).
  - §9 Logging from the CLIs → no implementation required (negative requirement; stderr-only is the natural path).
  - §10 Tests → unit tests embedded in Tasks 3, 4, 5, 6, 7, 8, 9; e2e in Task 10.
  - §11 Spec impact → Task 11.
  - §12 Risks → engine::cwd visibility (Task 5 Step 5); follow file truncation (no special handling — covered by Task 7's open-handle approach).

- **Placeholder scan:** No "TBD", "TODO", "fill in details" placeholders. Each code block is complete in itself. The single deliberate stub in Task 8 (`run_online_dispatch` exits 70) is replaced in Task 9.

- **Type consistency:** `LogArgs`, `EventsArgs`, `RepoArgs` field names, `Stream`, `Format`, and `Filter` types are referenced identically across tasks. `run_capture` is the test-only accessor (`#[cfg(test)] pub(crate)`); `run` is the production entry point. Sink trait and its two implementations are introduced once (Task 9) and only used there. `ApiEvent` / `EventsPage` are pulled from `roki_api_types` everywhere.
