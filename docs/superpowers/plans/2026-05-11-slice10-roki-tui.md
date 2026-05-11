# Slice 10 roki-tui Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `fr:11` end to end — standalone `ratatui` binary `roki-tui` that polls the slice-9 observability HTTP API and renders four views (tickets, ticket detail, events, escalations) with local-only escalation ack and a manual refresh action. Ship the additive `CycleSummary::last_state_id` field that the ticket-detail tail needs.

**Architecture:** New workspace member `roki-tui` (binary crate). One `tokio` multi-thread runtime drives three independent poll loops (`tickets`, `events`, `escalations`) plus an on-demand ticket-detail loop. All four send `Update` messages into a single `mpsc` channel that the render loop drains before each `ratatui::Terminal::draw`. The render loop is the sole owner of `AppModel`. HTTP layer is a thin `reqwest::Client` wrapper that only speaks the types in `roki-api-types`. `crossterm` events flow into the same channel as `Update::Input(KeyEvent)` so ordering is deterministic.

**Tech Stack:** Rust 2024 (workspace edition). `tokio = "1"` (full), `reqwest = "0.12"` (rustls-tls + json), `serde = "1"`, `serde_json = "1"`, `toml = "0.8"`, `time = "0.3"`, `uuid = "1"`, `clap = "4"` (derive), `tracing = "0.1"`, `thiserror = "2"`, `anyhow = "1"`, `ratatui = "0.28"` (crossterm backend), `crossterm = "0.28"`, `anstyle-parse = "0.2"` (ANSI strip state machine), `unicode-width = "0.1"`, `directories = "5"`. Test deps: `wiremock = "0.6"`, `tempfile = "3"`, `tokio` `test-util` feature.

**Spec:** `docs/superpowers/specs/2026-05-11-slice10-roki-tui-design.md`.

**Working branch:** `feature/slice10-roki-tui` (already created; spec committed). Every implementation commit lands here.

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-tui/Cargo.toml` | Workspace member manifest. |
| `crates/roki-tui/src/main.rs` | `tokio::main`, terminal setup/restore, `App::run` entrypoint. |
| `crates/roki-tui/src/lib.rs` | Library facade so integration tests can call `App::run_for_test`. |
| `crates/roki-tui/src/app.rs` | `App` struct, `App::run`, `App::run_for_test`. |
| `crates/roki-tui/src/cli.rs` | `clap::Parser` for positional `<API_URL>` + `--*-cadence` + `--config`. |
| `crates/roki-tui/src/config.rs` | `TuiConfig`, `PollingSection`, TOML loader, validation, CLI merge. |
| `crates/roki-tui/src/client/mod.rs` | `ApiClient`, `ClientError`. |
| `crates/roki-tui/src/client/tickets.rs` | `fetch_tickets`, `fetch_ticket_detail`. |
| `crates/roki-tui/src/client/cycles.rs` | `fetch_cycles`, `fetch_visit_stdout`. |
| `crates/roki-tui/src/client/events.rs` | `fetch_events_since`. |
| `crates/roki-tui/src/client/escalations.rs` | `fetch_escalations`. |
| `crates/roki-tui/src/client/refresh.rs` | `post_refresh`. |
| `crates/roki-tui/src/model/mod.rs` | `AppModel`, `View`, `Update`. |
| `crates/roki-tui/src/model/tickets.rs` | `TicketsView`, sort + selection reducer. |
| `crates/roki-tui/src/model/ticket_detail.rs` | `TicketDetailView`, cycle selection. |
| `crates/roki-tui/src/model/events.rs` | `EventsView`, `merge_page`, `gap_pending`. |
| `crates/roki-tui/src/model/escalations.rs` | `EscalationsView`, `AckKey`, `apply_escalations`. |
| `crates/roki-tui/src/model/status.rs` | `StatusLine`, `RefreshState`. |
| `crates/roki-tui/src/poll/mod.rs` | `PollScheduler` orchestration. |
| `crates/roki-tui/src/poll/tickets.rs` | tickets cadence loop. |
| `crates/roki-tui/src/poll/events.rs` | events cadence loop (tracks `last_seq`). |
| `crates/roki-tui/src/poll/escalations.rs` | escalations cadence loop. |
| `crates/roki-tui/src/poll/ticket_detail.rs` | on-demand ticket-detail loop. |
| `crates/roki-tui/src/ui/mod.rs` | `draw(frame, model)` dispatch. |
| `crates/roki-tui/src/ui/tickets.rs` | tickets `Table` widget. |
| `crates/roki-tui/src/ui/ticket_detail.rs` | split layout: cycles table + stdout tail. |
| `crates/roki-tui/src/ui/events.rs` | events `List`. |
| `crates/roki-tui/src/ui/escalations.rs` | escalations `Table` with ack glyph. |
| `crates/roki-tui/src/ui/status_bar.rs` | bottom status line + tab strip. |
| `crates/roki-tui/src/input.rs` | crossterm key → `Action` dispatch table. |
| `crates/roki-tui/src/sanitize.rs` | `ansi_strip`, `control_strip`, `clean_json`. |
| `crates/roki-tui/src/palette.rs` | `Palette`, `EnvProbe`, `detect_with`. |
| `crates/roki-tui/src/startup_log.rs` | JSON-line stderr emitter at startup. |
| `crates/roki-tui/tests/tui_smoke.rs` | wiremock-backed integration test of the model loop. |
| `crates/roki-tui/tests/tui_refresh_ack.rs` | refresh action exercise. |
| `crates/roki-tui/tests/tui_palette_fallback.rs` | palette detection with synthetic env. |
| `crates/roki-daemon/tests/e2e/cycles_last_state_id_smoke.rs` | new daemon e2e for `CycleSummary::last_state_id`. |

### Modified

| Path | Change |
|---|---|
| `Cargo.toml` (workspace root) | `members` gains `"crates/roki-tui"`. |
| `crates/roki-api-types/src/tickets.rs` | Add `CycleSummary::last_state_id: Option<String>` + round-trip test. |
| `crates/roki-daemon/src/api/projection/cycles.rs` | Populate `last_state_id` from `d.states.last().cloned()`. |
| `crates/roki-daemon/Cargo.toml` | Register `cycles_last_state_id_smoke` `[[test]]`. |
| `docs/reference/cli.md` | Add `roki-tui` subcommand row in the top table and a `## roki-tui` section with the four flags. |

---

## Task 0: Confirm spec + branch

**Files:** none.

- [ ] **Step 1: Verify branch**

Run: `git branch --show-current`
Expected: `feature/slice10-roki-tui`

- [ ] **Step 2: Verify spec exists**

Run: `ls docs/superpowers/specs/2026-05-11-slice10-roki-tui-design.md`
Expected: file listed.

- [ ] **Step 3: Verify slice 9 baseline**

Run: `cargo test --workspace --no-run`
Expected: builds clean. (If not, slice 9 regressed; stop and fix before continuing.)

**Acceptance:** branch confirmed, spec present, workspace compiles.

---

## Task 1: `CycleSummary::last_state_id` additive field

**Files:**
- Modify: `crates/roki-api-types/src/tickets.rs`

- [ ] **Step 1: Add the field + round-trip test**

Open `crates/roki-api-types/src/tickets.rs`. In `CycleSummary`, append:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_state_id: Option<String>,
```

…immediately after `pub failure_kind: Option<String>,` (keep the field above `total_visits` to keep related-ness grouped).

In the `tests` module append:

```rust
    #[test]
    fn cycle_summary_last_state_id_round_trips() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let with_value = CycleSummary {
            cycle_id: Uuid::nil(),
            kind: CycleKind::Rule,
            trigger: CycleTrigger::Runtime,
            started_at: now,
            ended_at: None,
            terminal_id: None,
            failure_kind: None,
            last_state_id: Some("post0".into()),
            total_visits: 3,
        };
        let json = serde_json::to_string(&with_value).unwrap();
        assert!(json.contains("\"last_state_id\":\"post0\""));
        let parsed: CycleSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(with_value, parsed);

        let without_value = CycleSummary {
            cycle_id: Uuid::nil(),
            kind: CycleKind::Rule,
            trigger: CycleTrigger::Runtime,
            started_at: now,
            ended_at: None,
            terminal_id: None,
            failure_kind: None,
            last_state_id: None,
            total_visits: 0,
        };
        let json = serde_json::to_string(&without_value).unwrap();
        assert!(!json.contains("last_state_id"));
        let parsed: CycleSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(without_value, parsed);
    }
```

- [ ] **Step 2: Run test**

Run: `cargo test -p roki-api-types tickets::tests::cycle_summary_last_state_id_round_trips -- --exact`
Expected: PASS.

- [ ] **Step 3: Update every existing constructor of `CycleSummary` so the workspace builds**

Run: `cargo build --workspace`. Expect failures at every `CycleSummary { ... }` literal. For each failure, insert `last_state_id: None,`. Likely locations:

- `crates/roki-daemon/src/api/projection/cycles.rs` (the `parse` function — Task 2 will overwrite this).
- Any test fixtures inside the daemon that build `CycleSummary` directly.

After the build is clean, run: `cargo test --workspace --no-run`. Expect a clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-api-types/src/tickets.rs $(git diff --name-only)
git commit -m "feat(slice10,api-types): add CycleSummary.last_state_id"
```

**Acceptance:** `cargo test -p roki-api-types` passes; `cargo build --workspace` passes; the workspace's slice-9 e2e tests still build.

---

## Task 2: project `last_state_id` from `cycle.json`

**Files:**
- Modify: `crates/roki-daemon/src/api/projection/cycles.rs`

- [ ] **Step 1: Update `parse`**

In `crates/roki-daemon/src/api/projection/cycles.rs`, replace the body of `fn parse(d: OnDisk) -> CycleSummary` with:

```rust
fn parse(d: OnDisk) -> CycleSummary {
    let last_state_id = d.states.last().cloned();
    CycleSummary {
        cycle_id: d.cycle_id,
        kind: match d.kind.as_str() {
            "rule" => CycleKind::Rule,
            "cleanup" => CycleKind::Cleanup,
            _ => CycleKind::Failure,
        },
        trigger: match d.trigger.as_str() {
            "cold_start" => CycleTrigger::ColdStart,
            _ => CycleTrigger::Runtime,
        },
        started_at: d.started_at,
        ended_at: d.ended_at,
        terminal_id: d.terminal_id,
        failure_kind: d.failure_kind,
        last_state_id,
        total_visits: d.total_visits,
    }
}
```

- [ ] **Step 2: Extend the in-module test**

In the same file, replace `fn lists_cycles_descending_by_started_at` with:

```rust
    #[test]
    fn lists_cycles_descending_by_started_at_and_populates_last_state_id() {
        let dir = TempDir::new().unwrap();
        let ticket = dir.path().join("ENG-1");
        let mut ids = vec![];
        for (i, ts) in ["2026-05-01T00:00:00Z", "2026-05-02T00:00:00Z"]
            .iter()
            .enumerate()
        {
            let id = Uuid::new_v4();
            ids.push(id);
            let cycle = ticket.join(format!("cycle-{id}"));
            std::fs::create_dir_all(&cycle).unwrap();
            let body = format!(
                r#"{{"cycle_id":"{id}","kind":"rule","trigger":"runtime","started_at":"{ts}","total_visits":1,"states":["pre","post{i}"]}}"#
            );
            std::fs::write(cycle.join("cycle.json"), body).unwrap();
        }
        let (cycles, truncated) = list_cycles(dir.path(), "ENG-1", 10);
        assert_eq!(cycles.len(), 2);
        assert!(cycles[0].started_at > cycles[1].started_at);
        assert_eq!(cycles[0].last_state_id.as_deref(), Some("post1"));
        assert_eq!(cycles[1].last_state_id.as_deref(), Some("post0"));
        assert!(!truncated);
    }

    #[test]
    fn last_state_id_is_none_when_states_array_is_empty() {
        let dir = TempDir::new().unwrap();
        let ticket = dir.path().join("ENG-2");
        let id = Uuid::new_v4();
        let cycle = ticket.join(format!("cycle-{id}"));
        std::fs::create_dir_all(&cycle).unwrap();
        let body = format!(
            r#"{{"cycle_id":"{id}","kind":"rule","trigger":"runtime","started_at":"2026-05-01T00:00:00Z","total_visits":0,"states":[]}}"#
        );
        std::fs::write(cycle.join("cycle.json"), body).unwrap();
        let (cycles, _) = list_cycles(dir.path(), "ENG-2", 10);
        assert_eq!(cycles.len(), 1);
        assert!(cycles[0].last_state_id.is_none());
    }
```

- [ ] **Step 3: Run unit tests**

Run: `cargo test -p roki-daemon api::projection::cycles::tests`
Expected: both tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/api/projection/cycles.rs
git commit -m "feat(slice10,daemon): project CycleSummary.last_state_id from cycle.json"
```

**Acceptance:** projection tests pass; existing slice-9 cycles test still passes.

---

## Task 3: e2e `cycles_last_state_id_smoke`

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cycles_last_state_id_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Register the test target**

Open `crates/roki-daemon/Cargo.toml`. In the `[[test]]` cluster (after the existing slice-9 `api_bind_failure_smoke` entry) append:

```toml
[[test]]
name = "cycles_last_state_id_smoke"
path = "tests/e2e/cycles_last_state_id_smoke.rs"
```

- [ ] **Step 2: Pick an existing slice-9 cycles fixture as a starting template**

Run: `ls crates/roki-daemon/tests/e2e/api_cycle_and_visit_smoke.rs`
Expected: file present. Open it and use it as the reference for: (a) how to spin up the daemon with `[api]` enabled, (b) how to fire a webhook, (c) how to wait for `cycle_completed`, (d) how to issue an HTTP `GET` against the local API.

- [ ] **Step 3: Write the fixture**

Create `crates/roki-daemon/tests/e2e/cycles_last_state_id_smoke.rs` modeled on `api_cycle_and_visit_smoke.rs`. The test must:

1. Configure a workflow whose rule visits two states (`pre`, then `post0`) and terminates at `post0`.
2. Fire one webhook driving the rule to completion.
3. `GET /api/tickets/{id}/cycles`.
4. Assert that exactly one cycle is returned and that:
   - `terminal_id == Some("post0")`
   - `last_state_id == Some("post0")`
5. Assert no panics, no extra cycles, and that `last_state_id` is serialized in the JSON body (string match on `"last_state_id":"post0"`).

If the existing fixture already exercises a workflow with two states, mirror the same fixture layout (TOML + WORKFLOW.yaml content + workflow markdown files). Copy them inline so the new fixture is self-contained per the slice-9 fixture style.

- [ ] **Step 4: Run**

Run: `cargo test -p roki-daemon --test cycles_last_state_id_smoke -- --nocapture`
Expected: PASS within ~30s on a warm cache.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/tests/e2e/cycles_last_state_id_smoke.rs
git commit -m "test(slice10,api,e2e): cycles_last_state_id_smoke"
```

**Acceptance:** fixture passes; no slice-9 fixture regressed (`cargo test -p roki-daemon --tests` clean).

---

## Task 4: scaffold `roki-tui` crate

**Files:**
- Create: `crates/roki-tui/Cargo.toml`, `crates/roki-tui/src/main.rs`, `crates/roki-tui/src/lib.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Register workspace member**

Open the workspace `Cargo.toml`. Replace:

```toml
members = ["crates/roki-daemon", "crates/roki-api-types"]
```

with:

```toml
members = ["crates/roki-daemon", "crates/roki-api-types", "crates/roki-tui"]
```

- [ ] **Step 2: Write `crates/roki-tui/Cargo.toml`**

```toml
[package]
name = "roki-tui"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "Terminal UI for the roki daemon's observability HTTP API"

[[bin]]
name = "roki-tui"
path = "src/main.rs"

[lib]
path = "src/lib.rs"

[dependencies]
roki-api-types = { path = "../roki-api-types" }
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
time = { version = "0.3", features = ["formatting", "parsing", "serde-well-known"] }
uuid = { version = "1", features = ["serde"] }
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "signal"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
ratatui = { version = "0.28", default-features = false, features = ["crossterm"] }
crossterm = "0.28"
tracing = "0.1"
anstyle-parse = "0.2"
unicode-width = "0.1"
directories = "5"

[dev-dependencies]
wiremock = "0.6"
tempfile = "3"
tokio = { version = "1", features = ["full", "test-util"] }

[lints]
workspace = true
```

- [ ] **Step 3: Write `crates/roki-tui/src/lib.rs`**

```rust
//! Library facade for the roki-tui binary. Integration tests in
//! `crates/roki-tui/tests/` consume `App::run_for_test`.

pub mod app;
pub mod cli;
pub mod client;
pub mod config;
pub mod input;
pub mod model;
pub mod palette;
pub mod poll;
pub mod sanitize;
pub mod startup_log;
pub mod ui;
```

- [ ] **Step 4: Write a minimal `crates/roki-tui/src/main.rs`**

```rust
use anyhow::Result;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    #[cfg(windows)]
    {
        eprintln!("roki-tui: Windows is not supported in v1");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        roki_tui::app::App::run().await
    }
}
```

- [ ] **Step 5: Write stub modules so the crate compiles**

For each of `app.rs`, `cli.rs`, `client/mod.rs`, `config.rs`, `input.rs`, `model/mod.rs`, `palette.rs`, `poll/mod.rs`, `sanitize.rs`, `startup_log.rs`, `ui/mod.rs`, create the file with a single line `//! filled in by Task <N>`. For `app.rs` write:

```rust
//! filled in by Task 15
use anyhow::{anyhow, Result};

pub struct App;

impl App {
    pub async fn run() -> Result<()> {
        Err(anyhow!("App::run unimplemented"))
    }
}
```

- [ ] **Step 6: Build**

Run: `cargo build -p roki-tui`
Expected: warnings about empty modules; **no errors**.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/roki-tui/
git commit -m "feat(slice10,tui): scaffold roki-tui crate"
```

**Acceptance:** workspace builds; `cargo metadata --format-version=1 | jq -r '.workspace_members[]' | grep roki-tui` returns one entry.

---

## Task 5: CLI parser

**Files:**
- Modify: `crates/roki-tui/src/cli.rs`

- [ ] **Step 1: Replace stub with the parser**

```rust
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
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p roki-tui cli::tests`
Expected: 3 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-tui/src/cli.rs
git commit -m "feat(slice10,tui): clap CLI parser"
```

**Acceptance:** CLI tests pass.

---

## Task 6: config loader

**Files:**
- Modify: `crates/roki-tui/src/config.rs`

- [ ] **Step 1: Replace stub with the loader**

```rust
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TuiConfig {
    pub polling: PollingSection,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self { polling: PollingSection::default() }
    }
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
    fn default_tickets() -> u32 { 2 }
    fn default_events() -> u32 { 1 }
    fn default_escalations() -> u32 { 5 }
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
        return Err(ConfigError::InvalidApiUrl(format!("{}: empty host", cli.api_url)));
    }

    let config = load_config_file(cli.config.as_deref())?;

    let mut polling = config.polling;
    if let Some(v) = cli.tickets_cadence { polling.tickets_seconds = v; }
    if let Some(v) = cli.events_cadence { polling.events_seconds = v; }
    if let Some(v) = cli.escalations_cadence { polling.escalations_seconds = v; }

    if polling.tickets_seconds < 1 {
        return Err(ConfigError::InvalidTicketsCadence(polling.tickets_seconds));
    }
    if polling.events_seconds < 1 {
        return Err(ConfigError::InvalidEventsCadence(polling.events_seconds));
    }
    if polling.escalations_seconds < 1 {
        return Err(ConfigError::InvalidEscalationsCadence(polling.escalations_seconds));
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
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p roki-tui config::tests`
Expected: all 6 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-tui/src/config.rs
git commit -m "feat(slice10,tui): config loader with CLI merge"
```

**Acceptance:** config tests pass.

---

## Task 7: sanitize layer

**Files:**
- Modify: `crates/roki-tui/src/sanitize.rs`

- [ ] **Step 1: Replace stub**

```rust
//! Defense-in-depth sanitization on TUI received strings (fr:11
//! §Defense-in-depth sanitization). Even though the server already sanitizes,
//! we strip again before storing or rendering.

use anstyle_parse::{Params, Parser, Perform};

/// Remove ANSI escape sequences (CSI / OSC / SGR / etc.) while preserving
/// printable bytes. Treats invalid UTF-8 by replacing with U+FFFD upstream;
/// this function operates on &str.
pub fn ansi_strip(s: &str) -> String {
    struct Sink {
        out: String,
    }
    impl Perform for Sink {
        fn print(&mut self, c: char) {
            self.out.push(c);
        }
        fn execute(&mut self, byte: u8) {
            // Preserve newline + tab; drop other C0 controls + DEL.
            if byte == b'\n' || byte == b'\t' {
                self.out.push(byte as char);
            }
        }
        fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: u8) {}
        fn put(&mut self, _: u8) {}
        fn unhook(&mut self) {}
        fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {}
        fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
        fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {}
    }
    let mut parser = Parser::new();
    let mut sink = Sink { out: String::with_capacity(s.len()) };
    for &byte in s.as_bytes() {
        parser.advance(&mut sink, byte);
    }
    sink.out
}

/// Drop C0 control characters except \n and \t plus DEL (0x7F).
pub fn control_strip(s: &str) -> String {
    s.chars()
        .filter(|c| {
            let cp = *c as u32;
            !(cp < 0x20 && cp != 0x09 && cp != 0x0A) && cp != 0x7F
        })
        .collect()
}

/// Recursively `ansi_strip` + `control_strip` every string leaf inside a JSON
/// value in place. Non-string leaves are untouched.
pub fn clean_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            *s = control_strip(&ansi_strip(s));
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(clean_json),
        serde_json::Value::Object(map) => map.values_mut().for_each(clean_json),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi() {
        let s = "\x1b[31mhello\x1b[0m world";
        assert_eq!(ansi_strip(s), "hello world");
    }

    #[test]
    fn strips_osc_with_bel_terminator() {
        let s = "before\x1b]0;title\x07after";
        assert_eq!(ansi_strip(s), "beforeafter");
    }

    #[test]
    fn keeps_newlines_and_tabs() {
        let s = "a\nb\tc";
        assert_eq!(ansi_strip(s), "a\nb\tc");
    }

    #[test]
    fn ansi_strip_handles_utf8_boundary() {
        let s = "\u{1F600}\x1b[31mhi";
        assert_eq!(ansi_strip(s), "\u{1F600}hi");
    }

    #[test]
    fn control_strip_drops_bel_and_null() {
        let s = "a\x00b\x07c";
        assert_eq!(control_strip(s), "abc");
    }

    #[test]
    fn control_strip_keeps_newline_tab() {
        assert_eq!(control_strip("x\ny\tz"), "x\ny\tz");
    }

    #[test]
    fn control_strip_drops_del() {
        assert_eq!(control_strip("a\x7Fb"), "ab");
    }

    #[test]
    fn clean_json_walks_nested() {
        let mut v = serde_json::json!({
            "a": "\x1b[31mhi\x07",
            "b": [{"c": "\x00x"}, 1, null]
        });
        clean_json(&mut v);
        assert_eq!(v["a"], serde_json::Value::String("hi".into()));
        assert_eq!(v["b"][0]["c"], serde_json::Value::String("x".into()));
        assert_eq!(v["b"][1], serde_json::json!(1));
        assert_eq!(v["b"][2], serde_json::Value::Null);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p roki-tui sanitize::tests`
Expected: 8 PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-tui/src/sanitize.rs
git commit -m "feat(slice10,tui): defense-in-depth ansi_strip + control_strip + clean_json"
```

**Acceptance:** sanitize tests pass.

---

## Task 8: palette detection

**Files:**
- Modify: `crates/roki-tui/src/palette.rs`

- [ ] **Step 1: Replace stub**

```rust
//! Terminal palette detection (fr:11 §Terminal compatibility).
//!
//! Detection order, first match wins:
//! 1. $COLORTERM ∈ {truecolor, 24bit} → Rgb24
//! 2. $TERM matches *-256color           → IndexedAnsi256
//! 3. otherwise                           → IndexedAnsi16

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    Rgb24,
    IndexedAnsi256,
    IndexedAnsi16,
}

impl Palette {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rgb24 => "rgb24",
            Self::IndexedAnsi256 => "indexed_ansi256",
            Self::IndexedAnsi16 => "indexed_ansi16",
        }
    }
}

pub trait EnvProbe {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnv;
impl EnvProbe for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

pub fn detect() -> Palette {
    detect_with(&ProcessEnv)
}

pub fn detect_with(env: &dyn EnvProbe) -> Palette {
    if let Some(v) = env.get("COLORTERM") {
        let v = v.to_ascii_lowercase();
        if v == "truecolor" || v == "24bit" {
            return Palette::Rgb24;
        }
    }
    if let Some(t) = env.get("TERM") {
        if t.ends_with("-256color") {
            return Palette::IndexedAnsi256;
        }
    }
    Palette::IndexedAnsi16
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapEnv(HashMap<&'static str, &'static str>);
    impl EnvProbe for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).map(|s| (*s).to_string())
        }
    }

    fn env(pairs: &[(&'static str, &'static str)]) -> MapEnv {
        MapEnv(pairs.iter().copied().collect())
    }

    #[test]
    fn colorterm_truecolor_wins() {
        assert_eq!(
            detect_with(&env(&[("COLORTERM", "truecolor"), ("TERM", "xterm")])),
            Palette::Rgb24
        );
    }

    #[test]
    fn term_256color_falls_back_to_indexed() {
        assert_eq!(
            detect_with(&env(&[("TERM", "xterm-256color")])),
            Palette::IndexedAnsi256
        );
    }

    #[test]
    fn dumb_falls_back_to_16() {
        assert_eq!(detect_with(&env(&[("TERM", "dumb")])), Palette::IndexedAnsi16);
    }

    #[test]
    fn empty_env_falls_back_to_16() {
        assert_eq!(detect_with(&env(&[])), Palette::IndexedAnsi16);
    }

    #[test]
    fn colorterm_case_insensitive() {
        assert_eq!(
            detect_with(&env(&[("COLORTERM", "TrueColor")])),
            Palette::Rgb24
        );
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p roki-tui palette::tests`
Expected: 5 PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-tui/src/palette.rs
git commit -m "feat(slice10,tui): palette detection with EnvProbe"
```

**Acceptance:** palette tests pass.

---

## Task 9: HTTP client

**Files:**
- Modify: `crates/roki-tui/src/client/mod.rs`
- Create: `crates/roki-tui/src/client/{tickets,cycles,events,escalations,refresh}.rs`

- [ ] **Step 1: Write `client/mod.rs`**

```rust
//! Thin reqwest::Client wrapper. Every JSON response runs through
//! `sanitize::clean_json` before reaching the model. Every text-stream
//! response runs through `ansi_strip` + `control_strip`.

pub mod cycles;
pub mod escalations;
pub mod events;
pub mod refresh;
pub mod tickets;

use std::time::Duration;

use reqwest::Url;
use thiserror::Error;

use crate::sanitize;

#[derive(Debug, Clone)]
pub struct ApiClient {
    base: Url,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let base = Url::parse(base_url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .pool_idle_timeout(Some(Duration::from_secs(120)))
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .build()?;
        Ok(Self { base, http })
    }

    pub(crate) fn url(&self, path: &str) -> Result<Url, ClientError> {
        self.base
            .join(path)
            .map_err(|e| ClientError::InvalidUrl(e.to_string()))
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid_url: {0}")]
    InvalidUrl(String),
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),
    #[error("invalid_utf8 in text response")]
    InvalidUtf8,
}

pub(crate) async fn get_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: Url,
) -> Result<T, ClientError> {
    let resp = http.get(url).header("Accept", "application/json").send().await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(ClientError::Http {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).to_string(),
        });
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    sanitize::clean_json(&mut value);
    let parsed: T = serde_json::from_value(value)?;
    Ok(parsed)
}

pub(crate) async fn get_text(
    http: &reqwest::Client,
    url: Url,
) -> Result<String, ClientError> {
    let resp = http.get(url).send().await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(ClientError::Http {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).to_string(),
        });
    }
    let raw = std::str::from_utf8(&bytes).map_err(|_| ClientError::InvalidUtf8)?;
    Ok(crate::sanitize::control_strip(&crate::sanitize::ansi_strip(raw)))
}
```

- [ ] **Step 2: Write `client/tickets.rs`**

```rust
use roki_api_types::{TicketDetail, TicketSummary};

use super::{get_json, ApiClient, ClientError};

impl ApiClient {
    pub async fn fetch_tickets(&self) -> Result<Vec<TicketSummary>, ClientError> {
        let url = self.url("api/tickets")?;
        get_json(&self.http, url).await
    }

    pub async fn fetch_ticket_detail(&self, id: &str) -> Result<TicketDetail, ClientError> {
        let url = self.url(&format!("api/tickets/{id}"))?;
        get_json(&self.http, url).await
    }
}
```

- [ ] **Step 3: Write `client/cycles.rs`**

```rust
use roki_api_types::CycleSummary;
use uuid::Uuid;

use super::{get_json, get_text, ApiClient, ClientError};

impl ApiClient {
    pub async fn fetch_cycles(&self, id: &str) -> Result<Vec<CycleSummary>, ClientError> {
        let url = self.url(&format!("api/tickets/{id}/cycles"))?;
        get_json(&self.http, url).await
    }

    pub async fn fetch_visit_stdout(
        &self,
        id: &str,
        cycle: Uuid,
        visit_n: u32,
        state_id: &str,
    ) -> Result<String, ClientError> {
        let url = self.url(&format!(
            "api/tickets/{id}/cycles/{cycle}/visits/{visit_n}/{state_id}/stdout"
        ))?;
        get_text(&self.http, url).await
    }
}
```

- [ ] **Step 4: Write `client/events.rs`**

```rust
use roki_api_types::EventsPage;

use super::{get_json, ApiClient, ClientError};

impl ApiClient {
    pub async fn fetch_events_since(&self, since: Option<u64>) -> Result<EventsPage, ClientError> {
        let mut url = self.url("api/events")?;
        if let Some(s) = since {
            url.query_pairs_mut().append_pair("since", &s.to_string());
        }
        get_json(&self.http, url).await
    }
}
```

- [ ] **Step 5: Write `client/escalations.rs`**

```rust
use roki_api_types::ApiEscalation;

use super::{get_json, ApiClient, ClientError};

impl ApiClient {
    pub async fn fetch_escalations(&self) -> Result<Vec<ApiEscalation>, ClientError> {
        let url = self.url("api/escalations")?;
        get_json(&self.http, url).await
    }
}
```

- [ ] **Step 6: Write `client/refresh.rs`**

```rust
use roki_api_types::RefreshAck;

use super::{ApiClient, ClientError};

impl ApiClient {
    pub async fn post_refresh(&self) -> Result<RefreshAck, ClientError> {
        let url = self.url("api/refresh")?;
        let resp = self.http.post(url).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(ClientError::Http {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).to_string(),
            });
        }
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        crate::sanitize::clean_json(&mut value);
        let parsed: RefreshAck = serde_json::from_value(value)?;
        Ok(parsed)
    }
}
```

- [ ] **Step 7: Write `client/mod.rs` tests (append at bottom)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use roki_api_types::{ApiEscalation, ApiEvent, CycleSummary, EventsPage, RefreshAck, TicketDetail, TicketSummary};
    use time::OffsetDateTime;
    use uuid::Uuid;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ticket(id: &str) -> TicketSummary {
        TicketSummary {
            ticket_id: id.into(),
            repo: "github.com/x/y".into(),
            status: "in_progress".into(),
            labels: vec!["urgent".into()],
            assignee: "u".into(),
            in_flight_cycle_id: None,
            last_event_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        }
    }

    #[tokio::test]
    async fn fetch_tickets_ok() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tickets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![ticket("ENG-1")]))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let got = c.fetch_tickets().await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].ticket_id, "ENG-1");
    }

    #[tokio::test]
    async fn fetch_tickets_http_404() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tickets"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let err = c.fetch_tickets().await.unwrap_err();
        match err {
            ClientError::Http { status: 404, .. } => {}
            other => panic!("wrong err: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_events_since_appends_query() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .and(query_param("since", "42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(EventsPage {
                events: vec![],
                gap: false,
                next_since: Some(42),
            }))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let page = c.fetch_events_since(Some(42)).await.unwrap();
        assert_eq!(page.next_since, Some(42));
    }

    #[tokio::test]
    async fn fetch_visit_stdout_strips_ansi() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/api/tickets/ENG-1/cycles/00000000-0000-0000-0000-000000000000/visits/1/post0/stdout",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("\x1b[31mred\x1b[0m\nplain"))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let body = c
            .fetch_visit_stdout("ENG-1", Uuid::nil(), 1, "post0")
            .await
            .unwrap();
        assert_eq!(body, "red\nplain");
    }

    #[tokio::test]
    async fn fetch_escalations_sanitizes_payload() {
        let srv = MockServer::start().await;
        let esc = ApiEscalation {
            ticket_id: Some("ENG-1".into()),
            cycle_id: None,
            kind: "recursion_bound".into(),
            state_id: None,
            visit_n: None,
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            error_text: "\x1b[31mboom".into(),
            marker: "recursion_bound".into(),
        };
        Mock::given(method("GET"))
            .and(path("/api/escalations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![esc]))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let got = c.fetch_escalations().await.unwrap();
        assert_eq!(got[0].error_text, "boom");
    }

    #[tokio::test]
    async fn post_refresh_returns_ack() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/refresh"))
            .respond_with(ResponseTemplate::new(202).set_body_json(RefreshAck {
                coalesced: true,
                earliest_fire_at: None,
                backoff_active: false,
            }))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let ack = c.post_refresh().await.unwrap();
        assert!(ack.coalesced);
    }

    #[tokio::test]
    async fn fetch_cycles_round_trip() {
        let srv = MockServer::start().await;
        let cyc = CycleSummary {
            cycle_id: Uuid::nil(),
            kind: roki_api_types::CycleKind::Rule,
            trigger: roki_api_types::CycleTrigger::Runtime,
            started_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            ended_at: None,
            terminal_id: None,
            failure_kind: None,
            last_state_id: Some("post0".into()),
            total_visits: 1,
        };
        Mock::given(method("GET"))
            .and(path("/api/tickets/ENG-1/cycles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![cyc]))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let got = c.fetch_cycles("ENG-1").await.unwrap();
        assert_eq!(got[0].last_state_id.as_deref(), Some("post0"));
    }

    #[tokio::test]
    async fn fetch_ticket_detail_round_trip() {
        let srv = MockServer::start().await;
        let detail = TicketDetail {
            summary: ticket("ENG-1"),
            recent_events: vec![ApiEvent {
                seq: 1,
                ts: OffsetDateTime::from_unix_timestamp(0).unwrap(),
                event: "cycle_started".into(),
                ticket_id: Some("ENG-1".into()),
                cycle_id: None,
                payload: serde_json::json!({}),
            }],
            truncated: false,
        };
        Mock::given(method("GET"))
            .and(path("/api/tickets/ENG-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let got = c.fetch_ticket_detail("ENG-1").await.unwrap();
        assert_eq!(got.summary.ticket_id, "ENG-1");
        assert_eq!(got.recent_events.len(), 1);
    }
}
```

- [ ] **Step 8: Run tests**

Run: `cargo test -p roki-tui client::tests`
Expected: 8 PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/roki-tui/src/client/
git commit -m "feat(slice10,tui): reqwest API client with wiremock tests"
```

**Acceptance:** client tests pass.

---

## Task 10: app model + reducers

**Files:**
- Modify: `crates/roki-tui/src/model/mod.rs`
- Create: `crates/roki-tui/src/model/{tickets,ticket_detail,events,escalations,status}.rs`

- [ ] **Step 1: `model/mod.rs`**

```rust
//! AppModel holds every piece of UI state. Reducers in the submodules are
//! pure functions over snapshots so unit tests do not need any I/O.

pub mod escalations;
pub mod events;
pub mod status;
pub mod ticket_detail;
pub mod tickets;

use std::time::Instant;

use crossterm::event::KeyEvent;
use roki_api_types::{
    ApiEscalation, ApiEvent, CycleSummary, EventsPage, RefreshAck, TicketDetail, TicketSummary,
};

use crate::palette::Palette;

pub use escalations::{AckKey, EscalationsView};
pub use events::EventsView;
pub use status::{RefreshState, StatusLine};
pub use ticket_detail::TicketDetailView;
pub use tickets::TicketsView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Tickets,
    TicketDetail,
    Events,
    Escalations,
}

#[derive(Debug)]
pub enum Update {
    Tickets(Vec<TicketSummary>),
    TicketDetail(TicketDetail),
    Cycles(Vec<CycleSummary>),
    Tail { visit_n: u32, body: String },
    Events { page: EventsPage, requested_since: Option<u64> },
    Escalations(Vec<ApiEscalation>),
    RefreshAck(RefreshAck),
    Input(KeyEvent),
    PollError { source: PollSource, message: String },
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollSource {
    Tickets,
    Events,
    Escalations,
    TicketDetail,
    Refresh,
}

pub struct AppModel {
    pub focus: View,
    pub tickets: TicketsView,
    pub ticket_detail: TicketDetailView,
    pub events: EventsView,
    pub escalations: EscalationsView,
    pub status: StatusLine,
    pub refresh: RefreshState,
    pub palette: Palette,
    pub started_at: Instant,
}

impl AppModel {
    pub fn new(palette: Palette) -> Self {
        Self {
            focus: View::Tickets,
            tickets: TicketsView::default(),
            ticket_detail: TicketDetailView::default(),
            events: EventsView::default(),
            escalations: EscalationsView::default(),
            status: StatusLine::default(),
            refresh: RefreshState::Idle,
            palette,
            started_at: Instant::now(),
        }
    }

    pub fn focus_view(&mut self, v: View) {
        self.focus = v;
    }

    pub fn selected_ticket_id(&self) -> Option<&str> {
        self.tickets
            .rows
            .get(self.tickets.selected)
            .map(|t| t.ticket_id.as_str())
    }

    pub fn apply_ticket_detail(&mut self, detail: TicketDetail) {
        self.ticket_detail.detail = Some(detail);
    }

    pub fn apply_cycles(&mut self, cycles: Vec<CycleSummary>) {
        let prev_selected = self.ticket_detail.selected_cycle;
        self.ticket_detail.cycles = cycles;
        self.ticket_detail.selected_cycle = prev_selected
            .min(self.ticket_detail.cycles.len().saturating_sub(1));
    }

    pub fn apply_tail(&mut self, visit_n: u32, body: String) {
        self.ticket_detail.tail_visit_n = Some(visit_n);
        self.ticket_detail.tail_text = Some(body);
    }

    pub fn apply_refresh_ack(&mut self, ack: RefreshAck) {
        let parts = vec![
            format!("coalesced={}", ack.coalesced),
            format!("backoff_active={}", ack.backoff_active),
            match ack.earliest_fire_at {
                Some(t) => format!("fire_at={t}"),
                None => "fire_at=now".into(),
            },
        ];
        self.status.set(format!("refresh: {}", parts.join(" ")));
        self.refresh = RefreshState::DebouncedUntil(Instant::now() + std::time::Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    #[test]
    fn selected_ticket_id_returns_none_when_empty() {
        let m = AppModel::new(Palette::IndexedAnsi16);
        assert!(m.selected_ticket_id().is_none());
    }

    #[test]
    fn selected_ticket_id_returns_first() {
        let mut m = AppModel::new(Palette::IndexedAnsi16);
        m.tickets.rows = vec![TicketSummary {
            ticket_id: "ENG-1".into(),
            repo: "github.com/x/y".into(),
            status: "open".into(),
            labels: vec![],
            assignee: "u".into(),
            in_flight_cycle_id: None,
            last_event_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        }];
        assert_eq!(m.selected_ticket_id(), Some("ENG-1"));
    }
}
```

- [ ] **Step 2: `model/tickets.rs`**

```rust
use roki_api_types::TicketSummary;

#[derive(Debug, Default, Clone)]
pub struct TicketsView {
    pub rows: Vec<TicketSummary>,
    pub selected: usize,
}

impl TicketsView {
    /// Replace `rows`, sorting by last_event_at descending. Keeps the previous
    /// selection clamped to the new row count.
    pub fn apply(&mut self, mut rows: Vec<TicketSummary>) {
        rows.sort_by(|a, b| b.last_event_at.cmp(&a.last_event_at));
        let len = rows.len();
        self.rows = rows;
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn move_down(&mut self) {
        if !self.rows.is_empty() && self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn t(id: &str, ts: i64) -> TicketSummary {
        TicketSummary {
            ticket_id: id.into(),
            repo: "github.com/x/y".into(),
            status: "open".into(),
            labels: vec![],
            assignee: "u".into(),
            in_flight_cycle_id: None,
            last_event_at: OffsetDateTime::from_unix_timestamp(ts).unwrap(),
        }
    }

    #[test]
    fn sorts_descending_by_last_event() {
        let mut v = TicketsView::default();
        v.apply(vec![t("A", 1), t("B", 3), t("C", 2)]);
        let ids: Vec<_> = v.rows.iter().map(|t| t.ticket_id.as_str()).collect();
        assert_eq!(ids, vec!["B", "C", "A"]);
    }

    #[test]
    fn move_clamps_at_bounds() {
        let mut v = TicketsView::default();
        v.apply(vec![t("A", 1), t("B", 2)]);
        v.move_down();
        v.move_down();
        assert_eq!(v.selected, 1);
        v.move_up();
        v.move_up();
        assert_eq!(v.selected, 0);
    }
}
```

- [ ] **Step 3: `model/ticket_detail.rs`**

```rust
use roki_api_types::{CycleSummary, TicketDetail};

#[derive(Debug, Default, Clone)]
pub struct TicketDetailView {
    pub ticket_id: Option<String>,
    pub detail: Option<TicketDetail>,
    pub cycles: Vec<CycleSummary>,
    pub selected_cycle: usize,
    pub tail_text: Option<String>,
    pub tail_visit_n: Option<u32>,
}

impl TicketDetailView {
    pub fn focus_ticket(&mut self, id: String) {
        if self.ticket_id.as_deref() != Some(id.as_str()) {
            self.ticket_id = Some(id);
            self.detail = None;
            self.cycles.clear();
            self.selected_cycle = 0;
            self.tail_text = None;
            self.tail_visit_n = None;
        }
    }

    pub fn selected_cycle(&self) -> Option<&CycleSummary> {
        self.cycles.get(self.selected_cycle)
    }

    pub fn move_cycle_down(&mut self) {
        if !self.cycles.is_empty() && self.selected_cycle + 1 < self.cycles.len() {
            self.selected_cycle += 1;
        }
    }

    pub fn move_cycle_up(&mut self) {
        self.selected_cycle = self.selected_cycle.saturating_sub(1);
    }
}
```

- [ ] **Step 4: `model/events.rs`**

```rust
use std::collections::VecDeque;

use roki_api_types::{ApiEvent, EventsPage};

const MAX_ROWS: usize = 1000;

#[derive(Debug, Default, Clone)]
pub struct EventsView {
    pub rows: VecDeque<ApiEvent>,
    pub last_seq: Option<u64>,
    pub gap_pending: bool,
}

impl EventsView {
    pub fn merge_page(&mut self, page: EventsPage, _requested_since: Option<u64>) {
        if page.gap {
            self.gap_pending = true;
        }
        for ev in page.events {
            self.last_seq = Some(self.last_seq.map_or(ev.seq, |s| s.max(ev.seq)));
            self.rows.push_back(ev);
            if self.rows.len() > MAX_ROWS {
                self.rows.pop_front();
            }
        }
        if !page.gap && page.next_since.is_some() {
            self.gap_pending = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn ev(seq: u64) -> ApiEvent {
        ApiEvent {
            seq,
            ts: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            event: "x".into(),
            ticket_id: None,
            cycle_id: None,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn merges_in_order_and_tracks_seq() {
        let mut v = EventsView::default();
        v.merge_page(
            EventsPage { events: vec![ev(1), ev(2)], gap: false, next_since: Some(2) },
            None,
        );
        v.merge_page(
            EventsPage { events: vec![ev(3)], gap: false, next_since: Some(3) },
            Some(2),
        );
        assert_eq!(v.last_seq, Some(3));
        assert_eq!(v.rows.len(), 3);
        assert!(!v.gap_pending);
    }

    #[test]
    fn gap_flag_sticks_until_clean_page_arrives() {
        let mut v = EventsView::default();
        v.merge_page(
            EventsPage { events: vec![ev(10)], gap: true, next_since: Some(10) },
            Some(0),
        );
        assert!(v.gap_pending);
        v.merge_page(
            EventsPage { events: vec![ev(11)], gap: false, next_since: Some(11) },
            Some(10),
        );
        assert!(!v.gap_pending);
    }

    #[test]
    fn caps_rows_at_max() {
        let mut v = EventsView::default();
        let big: Vec<_> = (0..(MAX_ROWS as u64 + 5)).map(ev).collect();
        v.merge_page(
            EventsPage { events: big, gap: false, next_since: Some(MAX_ROWS as u64 + 4) },
            None,
        );
        assert_eq!(v.rows.len(), MAX_ROWS);
        assert_eq!(v.rows.front().unwrap().seq, 5);
    }
}
```

- [ ] **Step 5: `model/escalations.rs`**

```rust
use std::collections::HashSet;

use roki_api_types::ApiEscalation;
use uuid::Uuid;

#[derive(Debug, Default, Clone)]
pub struct EscalationsView {
    pub rows: Vec<ApiEscalation>,
    pub acked: HashSet<AckKey>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AckKey {
    pub marker: String,
    pub ticket_id: Option<String>,
    pub cycle_id: Option<Uuid>,
    pub kind: String,
    pub state_id: Option<String>,
    pub visit_n: Option<u32>,
}

impl AckKey {
    pub fn from(e: &ApiEscalation) -> Self {
        Self {
            marker: e.marker.clone(),
            ticket_id: e.ticket_id.clone(),
            cycle_id: e.cycle_id,
            kind: e.kind.clone(),
            state_id: e.state_id.clone(),
            visit_n: e.visit_n,
        }
    }
}

impl EscalationsView {
    pub fn apply(&mut self, rows: Vec<ApiEscalation>) {
        let new_keys: HashSet<_> = rows.iter().map(AckKey::from).collect();
        self.acked.retain(|k| new_keys.contains(k));
        let len = rows.len();
        self.rows = rows;
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn toggle_ack(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            let k = AckKey::from(row);
            if !self.acked.remove(&k) {
                self.acked.insert(k);
            }
        }
    }

    pub fn is_acked(&self, row: &ApiEscalation) -> bool {
        self.acked.contains(&AckKey::from(row))
    }

    pub fn move_down(&mut self) {
        if !self.rows.is_empty() && self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn e(kind: &str, marker: &str) -> ApiEscalation {
        ApiEscalation {
            ticket_id: Some("ENG-1".into()),
            cycle_id: None,
            kind: kind.into(),
            state_id: None,
            visit_n: None,
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            error_text: "boom".into(),
            marker: marker.into(),
        }
    }

    #[test]
    fn toggle_ack_round_trip() {
        let mut v = EscalationsView::default();
        v.apply(vec![e("recursion_bound", "recursion_bound")]);
        assert!(!v.is_acked(&v.rows[0]));
        v.toggle_ack();
        assert!(v.is_acked(&v.rows[0]));
        v.toggle_ack();
        assert!(!v.is_acked(&v.rows[0]));
    }

    #[test]
    fn ack_clears_when_entry_disappears() {
        let mut v = EscalationsView::default();
        v.apply(vec![e("recursion_bound", "recursion_bound")]);
        v.toggle_ack();
        assert_eq!(v.acked.len(), 1);
        v.apply(vec![e("cleanup_fs", "cleanup_fs")]);
        assert!(v.acked.is_empty());
    }

    #[test]
    fn ack_persists_when_entry_persists() {
        let mut v = EscalationsView::default();
        v.apply(vec![e("recursion_bound", "recursion_bound")]);
        v.toggle_ack();
        v.apply(vec![e("recursion_bound", "recursion_bound")]);
        assert_eq!(v.acked.len(), 1);
    }
}
```

- [ ] **Step 6: `model/status.rs`**

```rust
use std::time::Instant;

#[derive(Debug, Default, Clone)]
pub struct StatusLine {
    text: String,
}

impl StatusLine {
    pub fn set(&mut self, msg: impl Into<String>) {
        self.text = msg.into();
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, Copy)]
pub enum RefreshState {
    Idle,
    InFlight,
    DebouncedUntil(Instant),
}
```

- [ ] **Step 7: Run all model tests**

Run: `cargo test -p roki-tui model::`
Expected: every model unit test PASSes (10+ tests across submodules).

- [ ] **Step 8: Commit**

```bash
git add crates/roki-tui/src/model/
git commit -m "feat(slice10,tui): app model + reducers"
```

**Acceptance:** model tests pass.

---

## Task 11: poll scheduler

**Files:**
- Modify: `crates/roki-tui/src/poll/mod.rs`
- Create: `crates/roki-tui/src/poll/{tickets,events,escalations,ticket_detail}.rs`

- [ ] **Step 1: `poll/mod.rs`**

```rust
//! Three independent cadences plus an on-demand ticket-detail loop. All four
//! tasks send `Update` messages into a single `mpsc::Sender<Update>` so the
//! render loop has one ordering point.

pub mod escalations;
pub mod events;
pub mod ticket_detail;
pub mod tickets;

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::client::ApiClient;
use crate::config::PollingSection;
use crate::model::Update;

pub struct PollHandles {
    pub focus_tx: watch::Sender<Option<String>>,
}

pub fn spawn(
    client: Arc<ApiClient>,
    cfg: PollingSection,
    tx: mpsc::Sender<Update>,
) -> PollHandles {
    let (focus_tx, focus_rx) = watch::channel(None);
    tokio::spawn(tickets::run(client.clone(), cfg.tickets_seconds, tx.clone()));
    tokio::spawn(events::run(client.clone(), cfg.events_seconds, tx.clone()));
    tokio::spawn(escalations::run(client.clone(), cfg.escalations_seconds, tx.clone()));
    tokio::spawn(ticket_detail::run(client, cfg.tickets_seconds, focus_rx, tx));
    PollHandles { focus_tx }
}
```

- [ ] **Step 2: `poll/tickets.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;

use crate::client::ApiClient;
use crate::model::{PollSource, Update};

pub async fn run(client: Arc<ApiClient>, cadence_seconds: u32, tx: mpsc::Sender<Update>) {
    let mut interval = time::interval(Duration::from_secs(cadence_seconds as u64));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match client.fetch_tickets().await {
            Ok(rows) => {
                if tx.send(Update::Tickets(rows)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Update::PollError {
                        source: PollSource::Tickets,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }
}
```

- [ ] **Step 3: `poll/events.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;

use crate::client::ApiClient;
use crate::model::{PollSource, Update};

pub async fn run(client: Arc<ApiClient>, cadence_seconds: u32, tx: mpsc::Sender<Update>) {
    let mut interval = time::interval(Duration::from_secs(cadence_seconds as u64));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut last_seq: Option<u64> = None;
    loop {
        interval.tick().await;
        let requested = last_seq;
        match client.fetch_events_since(requested).await {
            Ok(page) => {
                last_seq = page.next_since.or(last_seq);
                if tx
                    .send(Update::Events { page, requested_since: requested })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Update::PollError {
                        source: PollSource::Events,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }
}
```

- [ ] **Step 4: `poll/escalations.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;

use crate::client::ApiClient;
use crate::model::{PollSource, Update};

pub async fn run(client: Arc<ApiClient>, cadence_seconds: u32, tx: mpsc::Sender<Update>) {
    let mut interval = time::interval(Duration::from_secs(cadence_seconds as u64));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match client.fetch_escalations().await {
            Ok(rows) => {
                if tx.send(Update::Escalations(rows)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Update::PollError {
                        source: PollSource::Escalations,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }
}
```

- [ ] **Step 5: `poll/ticket_detail.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time;

use crate::client::ApiClient;
use crate::model::{PollSource, Update};

pub async fn run(
    client: Arc<ApiClient>,
    cadence_seconds: u32,
    mut focus_rx: watch::Receiver<Option<String>>,
    tx: mpsc::Sender<Update>,
) {
    let mut interval = time::interval(Duration::from_secs(cadence_seconds as u64));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = focus_rx.changed() => {}
            _ = interval.tick() => {}
        }
        let Some(ticket_id) = focus_rx.borrow().clone() else { continue };
        let detail = client.fetch_ticket_detail(&ticket_id).await;
        let cycles = client.fetch_cycles(&ticket_id).await;
        match detail {
            Ok(d) => {
                if tx.send(Update::TicketDetail(d)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Update::PollError {
                        source: PollSource::TicketDetail,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
        match cycles {
            Ok(rows) => {
                let mut latest = rows.clone();
                latest.sort_by(|a, b| b.started_at.cmp(&a.started_at));
                let target = latest.into_iter().next();
                if tx.send(Update::Cycles(rows)).await.is_err() {
                    return;
                }
                if let Some(c) = target {
                    if let (Some(state_id), n) = (c.last_state_id.clone(), c.total_visits) {
                        if n > 0 {
                            match client
                                .fetch_visit_stdout(&ticket_id, c.cycle_id, n, &state_id)
                                .await
                            {
                                Ok(body) => {
                                    let _ = tx
                                        .send(Update::Tail { visit_n: n, body })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(Update::PollError {
                                            source: PollSource::TicketDetail,
                                            message: e.to_string(),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Update::PollError {
                        source: PollSource::TicketDetail,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }
}
```

- [ ] **Step 6: Unit test for tickets cadence using `tokio::time::pause`**

Append to `poll/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::client::ApiClient;
    use crate::model::Update;

    #[tokio::test(start_paused = true)]
    async fn tickets_poll_fires_on_cadence() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tickets"))
            .respond_with(ResponseTemplate::new(200).set_body_json::<Vec<roki_api_types::TicketSummary>>(vec![]))
            .mount(&srv)
            .await;
        let client = Arc::new(ApiClient::new(&srv.uri()).unwrap());
        let (tx, mut rx) = mpsc::channel::<Update>(8);
        let handle = tokio::spawn(super::tickets::run(client, 1, tx));
        // Advance virtual time past the first cadence tick.
        tokio::time::advance(Duration::from_secs(2)).await;
        // Yield so the inner future can run.
        tokio::task::yield_now().await;
        // We expect at least one Tickets update.
        let mut saw_update = false;
        while let Ok(Some(u)) = tokio::time::timeout(Duration::from_millis(10), rx.recv()).await {
            if matches!(u, Update::Tickets(_)) {
                saw_update = true;
                break;
            }
        }
        handle.abort();
        assert!(saw_update, "expected at least one Tickets update");
    }
}
```

(Note: `start_paused` plus `wiremock` interactions can be flaky under `MissedTickBehavior::Delay`. If the test is flaky in CI, replace `tokio::time::advance` with a real-time `sleep` of `Duration::from_millis(1100)` — slower but deterministic. Document the choice inline.)

- [ ] **Step 7: Run tests**

Run: `cargo test -p roki-tui poll::tests`
Expected: 1 PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-tui/src/poll/
git commit -m "feat(slice10,tui): cadence-driven poll scheduler"
```

**Acceptance:** poll tests pass.

---

## Task 12: ratatui widgets

**Files:**
- Modify: `crates/roki-tui/src/ui/mod.rs`
- Create: `crates/roki-tui/src/ui/{tickets,ticket_detail,events,escalations,status_bar}.rs`

- [ ] **Step 1: `ui/mod.rs`**

```rust
//! View dispatch + chrome (tab strip, status bar). Each submodule renders one
//! View into a Frame.

pub mod escalations;
pub mod events;
pub mod status_bar;
pub mod ticket_detail;
pub mod tickets;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::model::{AppModel, View};

pub fn draw(frame: &mut Frame, model: &AppModel) {
    let area = frame.size();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    status_bar::tab_strip(frame, layout[0], model);
    match model.focus {
        View::Tickets => tickets::draw(frame, layout[1], model),
        View::TicketDetail => ticket_detail::draw(frame, layout[1], model),
        View::Events => events::draw(frame, layout[1], model),
        View::Escalations => escalations::draw(frame, layout[1], model),
    }
    status_bar::status_line(frame, layout[2], model);
}
```

- [ ] **Step 2: `ui/tickets.rs`**

```rust
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

use crate::model::AppModel;

pub fn draw(frame: &mut Frame, area: Rect, model: &AppModel) {
    let header = Row::new(vec![
        "TicketId", "Repo", "Status", "Labels", "Assignee", "InFlight", "LastEvent",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = model.tickets.rows.iter().enumerate().map(|(i, t)| {
        let style = if i == model.tickets.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(t.ticket_id.clone()),
            Cell::from(t.repo.clone()),
            Cell::from(t.status.clone()),
            Cell::from(t.labels.join(",")),
            Cell::from(t.assignee.clone()),
            Cell::from(t.in_flight_cycle_id.map(|u| u.to_string()).unwrap_or_default()),
            Cell::from(t.last_event_at.to_string()),
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(14),
        Constraint::Length(28),
        Constraint::Length(14),
        Constraint::Length(20),
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(28),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title("Tickets").borders(Borders::ALL));
    frame.render_widget(table, area);
}

use ratatui::layout::Constraint;
```

- [ ] **Step 3: `ui/ticket_detail.rs`**

```rust
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::model::AppModel;

pub fn draw(frame: &mut Frame, area: Rect, model: &AppModel) {
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)])
        .split(area);

    let header =
        Row::new(vec!["CycleId", "Kind", "Trigger", "Started", "Ended", "Terminal", "Visits"])
            .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = model.ticket_detail.cycles.iter().enumerate().map(|(i, c)| {
        let style = if i == model.ticket_detail.selected_cycle {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(c.cycle_id.to_string()),
            Cell::from(format!("{:?}", c.kind).to_lowercase()),
            Cell::from(format!("{:?}", c.trigger).to_lowercase()),
            Cell::from(c.started_at.to_string()),
            Cell::from(c.ended_at.map(|t| t.to_string()).unwrap_or_default()),
            Cell::from(c.terminal_id.clone().unwrap_or_default()),
            Cell::from(c.total_visits.to_string()),
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(38),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Length(20),
        Constraint::Length(14),
        Constraint::Length(6),
    ];
    let cycles_table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title("Cycles").borders(Borders::ALL));
    frame.render_widget(cycles_table, split[0]);

    let body = model
        .ticket_detail
        .tail_text
        .clone()
        .unwrap_or_else(|| "(no tail available)".to_string());
    let tail = Paragraph::new(body)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(format!(
                    "Stdout tail (visit {})",
                    model
                        .ticket_detail
                        .tail_visit_n
                        .map(|v| v.to_string())
                        .unwrap_or_default()
                ))
                .borders(Borders::ALL),
        );
    frame.render_widget(tail, split[1]);
}
```

- [ ] **Step 4: `ui/events.rs`**

```rust
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;

use crate::model::AppModel;

pub fn draw(frame: &mut Frame, area: Rect, model: &AppModel) {
    let items: Vec<ListItem> = model
        .events
        .rows
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .map(|e| {
            ListItem::new(format!(
                "{} #{} {} {}{}",
                e.ts,
                e.seq,
                e.event,
                e.ticket_id.clone().unwrap_or_default(),
                e.cycle_id.map(|u| format!(" cycle={u}")).unwrap_or_default(),
            ))
        })
        .collect();
    let title = if model.events.gap_pending {
        "Events (gap pending — consult roki events --file <log>)"
    } else {
        "Events"
    };
    let list = List::new(items).block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(list, area);
}
```

- [ ] **Step 5: `ui/escalations.rs`**

```rust
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

use crate::model::AppModel;

pub fn draw(frame: &mut Frame, area: Rect, model: &AppModel) {
    let header = Row::new(vec!["Ack", "Kind", "StateId", "Ticket", "Cycle", "Error"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = model.escalations.rows.iter().enumerate().map(|(i, e)| {
        let glyph = if model.escalations.is_acked(e) { "[*]" } else { "[ ]" };
        let mut style = Style::default();
        if i == model.escalations.selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if model.escalations.is_acked(e) {
            style = style.add_modifier(Modifier::DIM);
        }
        Row::new(vec![
            Cell::from(glyph),
            Cell::from(e.kind.clone()),
            Cell::from(e.state_id.clone().unwrap_or_default()),
            Cell::from(e.ticket_id.clone().unwrap_or_default()),
            Cell::from(e.cycle_id.map(|u| u.to_string()).unwrap_or_default()),
            Cell::from(e.error_text.clone()),
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(3),
        Constraint::Length(18),
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(38),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title("Escalations").borders(Borders::ALL));
    frame.render_widget(table, area);
}

use ratatui::layout::Constraint;
```

- [ ] **Step 6: `ui/status_bar.rs`**

```rust
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::model::{AppModel, View};

pub fn tab_strip(frame: &mut Frame, area: Rect, model: &AppModel) {
    let tabs = [
        (View::Tickets, "[1]Tickets"),
        (View::TicketDetail, "[2]Detail"),
        (View::Events, "[3]Events"),
        (View::Escalations, "[4]Escalations"),
    ];
    let line: String = tabs
        .iter()
        .map(|(v, label)| {
            if *v == model.focus {
                format!("<{label}>")
            } else {
                format!(" {label} ")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    frame.render_widget(
        Paragraph::new(line).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

pub fn status_line(frame: &mut Frame, area: Rect, model: &AppModel) {
    frame.render_widget(Paragraph::new(model.status.text().to_string()), area);
}
```

- [ ] **Step 7: Smoke test the ratatui Buffer**

Append to `ui/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::model::AppModel;
    use crate::palette::Palette;
    use crate::ui::draw;

    #[test]
    fn draws_empty_tickets_view_without_panic() {
        let backend = TestBackend::new(120, 30);
        let mut term = Terminal::new(backend).unwrap();
        let model = AppModel::new(Palette::IndexedAnsi16);
        term.draw(|f| draw(f, &model)).unwrap();
        let buf = term.backend().buffer();
        let cell = buf.get(0, 0);
        assert!(!cell.symbol().is_empty());
    }
}
```

- [ ] **Step 8: Run tests**

Run: `cargo test -p roki-tui ui::tests`
Expected: 1 PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/roki-tui/src/ui/
git commit -m "feat(slice10,tui): ratatui widgets for all four views"
```

**Acceptance:** ui tests pass.

---

## Task 13: input dispatch

**Files:**
- Modify: `crates/roki-tui/src/input.rs`

- [ ] **Step 1: Replace stub**

```rust
//! crossterm key event → Action. Pure mapping; the App applies the Action.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::View;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Focus(View),
    Up,
    Down,
    Enter,
    Refresh,
    ToggleAck,
    PrintLogCmd,
    None,
}

pub fn classify(ev: KeyEvent) -> Action {
    if ev.modifiers.contains(KeyModifiers::CONTROL) && matches!(ev.code, KeyCode::Char('c')) {
        return Action::Quit;
    }
    match ev.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('1') => Action::Focus(View::Tickets),
        KeyCode::Char('2') => Action::Focus(View::TicketDetail),
        KeyCode::Char('3') => Action::Focus(View::Events),
        KeyCode::Char('4') => Action::Focus(View::Escalations),
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Enter => Action::Enter,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('a') => Action::ToggleAck,
        KeyCode::Char('c') => Action::PrintLogCmd,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn maps_focus_keys() {
        assert_eq!(classify(k('1')), Action::Focus(View::Tickets));
        assert_eq!(classify(k('4')), Action::Focus(View::Escalations));
    }

    #[test]
    fn maps_ctrl_c_to_quit() {
        let ev = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(classify(ev), Action::Quit);
    }

    #[test]
    fn maps_arrow_and_vi_motion() {
        assert_eq!(classify(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)), Action::Up);
        assert_eq!(classify(k('j')), Action::Down);
    }

    #[test]
    fn unknown_keys_are_none() {
        assert_eq!(classify(k('x')), Action::None);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p roki-tui input::tests`
Expected: 4 PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-tui/src/input.rs
git commit -m "feat(slice10,tui): key → action dispatch"
```

**Acceptance:** input tests pass.

---

## Task 14: startup log

**Files:**
- Modify: `crates/roki-tui/src/startup_log.rs`

- [ ] **Step 1: Replace stub**

```rust
//! Single JSON-line emitted to TUI's own stderr at startup (fr:11 §Logging).

use std::io::Write;

use crate::config::PollingSection;
use crate::palette::Palette;

pub fn emit<W: Write>(
    mut out: W,
    api_url: &str,
    polling: &PollingSection,
    palette: Palette,
) -> std::io::Result<()> {
    let value = serde_json::json!({
        "event": "roki_tui_started",
        "ts": now_rfc3339(),
        "api_url": api_url,
        "polling": {
            "tickets_seconds": polling.tickets_seconds,
            "events_seconds": polling.events_seconds,
            "escalations_seconds": polling.escalations_seconds,
        },
        "palette": palette.as_str(),
    });
    writeln!(out, "{value}")
}

pub fn emit_decode_error<W: Write>(mut out: W, endpoint: &str, error: &str) -> std::io::Result<()> {
    let value = serde_json::json!({
        "event": "roki_tui_decode_error",
        "ts": now_rfc3339(),
        "endpoint": endpoint,
        "error": error,
    });
    writeln!(out, "{value}")
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_writes_one_json_line() {
        let mut buf = Vec::new();
        let cfg = PollingSection::default();
        emit(&mut buf, "http://127.0.0.1:8080", &cfg, Palette::IndexedAnsi16).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert!(line.contains("\"event\":\"roki_tui_started\""));
        assert!(line.contains("\"palette\":\"indexed_ansi16\""));
        assert!(line.ends_with('\n'));
        // Must parse as a single JSON value.
        let trimmed = line.trim_end_matches('\n');
        let _: serde_json::Value = serde_json::from_str(trimmed).unwrap();
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p roki-tui startup_log::tests`
Expected: 1 PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-tui/src/startup_log.rs
git commit -m "feat(slice10,tui): structured startup log to stderr"
```

**Acceptance:** startup-log test passes.

---

## Task 15: App orchestration

**Files:**
- Modify: `crates/roki-tui/src/app.rs`, `crates/roki-tui/src/main.rs`

- [ ] **Step 1: Replace `app.rs` stub**

```rust
//! Top-level orchestration: terminal setup/restore, input forwarding, render
//! loop, Update reducer.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::cli::Cli;
use crate::client::ApiClient;
use crate::config::{resolve, ResolvedConfig};
use crate::input::{classify, Action};
use crate::model::{AppModel, PollSource, RefreshState, Update, View};
use crate::palette::{detect, Palette};
use crate::poll::{spawn as spawn_polls, PollHandles};
use crate::startup_log;
use crate::ui;

pub struct App;

impl App {
    pub async fn run() -> Result<()> {
        let cli = Cli::parse_args();
        let cfg = resolve(cli)?;
        let palette = detect();
        startup_log::emit(io::stderr(), &cfg.api_url, &cfg.polling, palette)?;
        run_inner(cfg, palette, /*headless=*/ false).await
    }

    /// Headless entry for integration tests. Drives the model loop without a
    /// real terminal. Returns the model after `max_updates` Update messages
    /// were processed or `timeout` elapsed.
    pub async fn run_for_test(
        api_url: &str,
        max_updates: usize,
        timeout: Duration,
    ) -> Result<AppModel> {
        let palette = detect();
        let cfg = ResolvedConfig {
            api_url: api_url.into(),
            polling: crate::config::PollingSection {
                tickets_seconds: 1,
                events_seconds: 1,
                escalations_seconds: 1,
            },
        };
        run_inner_headless(cfg, palette, max_updates, timeout).await
    }
}

async fn run_inner(cfg: ResolvedConfig, palette: Palette, _headless: bool) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = drive(cfg, palette, &mut terminal).await;
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    result
}

async fn drive(
    cfg: ResolvedConfig,
    palette: Palette,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let client = Arc::new(ApiClient::new(&cfg.api_url)?);
    let (tx, mut rx) = mpsc::channel::<Update>(64);
    let handles = spawn_polls(client.clone(), cfg.polling.clone(), tx.clone());
    let mut input_stream = EventStream::new();
    let input_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(Ok(ev)) = input_stream.next().await {
            if let Event::Key(k) = ev {
                if input_tx.send(Update::Input(k)).await.is_err() {
                    break;
                }
            }
        }
    });
    let mut model = AppModel::new(palette);
    loop {
        terminal.draw(|f| ui::draw(f, &model))?;
        let Some(update) = rx.recv().await else { break };
        if matches!(update, Update::Quit) {
            break;
        }
        if !apply(&mut model, update, &handles, client.clone(), tx.clone()) {
            break;
        }
    }
    Ok(())
}

async fn run_inner_headless(
    cfg: ResolvedConfig,
    palette: Palette,
    max_updates: usize,
    timeout: Duration,
) -> Result<AppModel> {
    let client = Arc::new(ApiClient::new(&cfg.api_url)?);
    let (tx, mut rx) = mpsc::channel::<Update>(64);
    let handles = spawn_polls(client.clone(), cfg.polling.clone(), tx.clone());
    let mut model = AppModel::new(palette);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = 0usize;
    while seen < max_updates {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(update)) => {
                if matches!(update, Update::Quit) {
                    break;
                }
                if !apply(&mut model, update, &handles, client.clone(), tx.clone()) {
                    break;
                }
                seen += 1;
            }
            _ => break,
        }
    }
    Ok(model)
}

fn apply(
    model: &mut AppModel,
    update: Update,
    handles: &PollHandles,
    client: Arc<ApiClient>,
    tx: mpsc::Sender<Update>,
) -> bool {
    match update {
        Update::Tickets(rows) => model.tickets.apply(rows),
        Update::TicketDetail(d) => model.apply_ticket_detail(d),
        Update::Cycles(rows) => model.apply_cycles(rows),
        Update::Tail { visit_n, body } => model.apply_tail(visit_n, body),
        Update::Events { page, requested_since } => model.events.merge_page(page, requested_since),
        Update::Escalations(rows) => model.escalations.apply(rows),
        Update::RefreshAck(ack) => model.apply_refresh_ack(ack),
        Update::PollError { source, message } => {
            model.status.set(format!("{:?}: {message}", source));
            if matches!(source, PollSource::Refresh) {
                model.refresh = RefreshState::Idle;
            }
        }
        Update::Input(ev) => {
            let action = classify(ev);
            return apply_action(model, action, handles, client, tx);
        }
        Update::Quit => return false,
    }
    true
}

fn apply_action(
    model: &mut AppModel,
    action: Action,
    handles: &PollHandles,
    client: Arc<ApiClient>,
    tx: mpsc::Sender<Update>,
) -> bool {
    match action {
        Action::Quit => return false,
        Action::Focus(View::TicketDetail) => {
            if let Some(id) = model.selected_ticket_id().map(str::to_string) {
                model.ticket_detail.focus_ticket(id.clone());
                let _ = handles.focus_tx.send(Some(id));
            }
            model.focus_view(View::TicketDetail);
        }
        Action::Focus(v) => model.focus_view(v),
        Action::Up => match model.focus {
            View::Tickets => model.tickets.move_up(),
            View::TicketDetail => model.ticket_detail.move_cycle_up(),
            View::Escalations => model.escalations.move_up(),
            _ => {}
        },
        Action::Down => match model.focus {
            View::Tickets => model.tickets.move_down(),
            View::TicketDetail => model.ticket_detail.move_cycle_down(),
            View::Escalations => model.escalations.move_down(),
            _ => {}
        },
        Action::Enter => {
            if model.focus == View::Tickets {
                if let Some(id) = model.selected_ticket_id().map(str::to_string) {
                    model.ticket_detail.focus_ticket(id.clone());
                    let _ = handles.focus_tx.send(Some(id));
                    model.focus_view(View::TicketDetail);
                }
            }
        }
        Action::Refresh => {
            match model.refresh {
                RefreshState::Idle => {
                    model.refresh = RefreshState::InFlight;
                    let c = client.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        match c.post_refresh().await {
                            Ok(ack) => {
                                let _ = tx.send(Update::RefreshAck(ack)).await;
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(Update::PollError {
                                        source: PollSource::Refresh,
                                        message: e.to_string(),
                                    })
                                    .await;
                            }
                        }
                    });
                }
                RefreshState::InFlight => {
                    model.status.set("refresh: already in flight");
                }
                RefreshState::DebouncedUntil(t) => {
                    let now = std::time::Instant::now();
                    let remaining = t.saturating_duration_since(now).as_secs();
                    if remaining == 0 {
                        model.refresh = RefreshState::Idle;
                        return apply_action(model, Action::Refresh, handles, client, tx);
                    }
                    model.status.set(format!("refresh: debounced ({remaining}s)"));
                }
            }
        }
        Action::ToggleAck => {
            if model.focus == View::Escalations {
                model.escalations.toggle_ack();
            }
        }
        Action::PrintLogCmd => {
            if model.focus == View::TicketDetail {
                if let (Some(ticket), Some(c)) =
                    (model.ticket_detail.ticket_id.clone(), model.ticket_detail.selected_cycle())
                {
                    let state = c.last_state_id.clone().unwrap_or_default();
                    let n = c.total_visits.max(1);
                    model.status.set(format!(
                        "roki log --ticket {ticket} --cycle {} --iter {n} --state {state} --stream stdout",
                        c.cycle_id
                    ));
                }
            }
        }
        Action::None => {}
    }
    true
}
```

- [ ] **Step 2: Add `futures-util` to deps**

Edit `crates/roki-tui/Cargo.toml`, append in `[dependencies]`:

```toml
futures-util = "0.3"
```

- [ ] **Step 3: Build**

Run: `cargo build -p roki-tui`
Expected: clean (no warnings beyond `unused` on the empty `_headless` parameter, which is fine — keep it for future plumbing).

- [ ] **Step 4: Commit**

```bash
git add crates/roki-tui/Cargo.toml crates/roki-tui/src/app.rs crates/roki-tui/src/main.rs
git commit -m "feat(slice10,tui): orchestrate poll + render + input loop"
```

**Acceptance:** crate builds; `roki-tui --help` runs (try `cargo run -p roki-tui -- --help`).

---

## Task 16: integration tests

**Files:**
- Create: `crates/roki-tui/tests/tui_smoke.rs`, `crates/roki-tui/tests/tui_refresh_ack.rs`, `crates/roki-tui/tests/tui_palette_fallback.rs`

- [ ] **Step 1: `tests/tui_smoke.rs`**

```rust
use std::time::Duration;

use roki_tui::app::App;
use time::OffsetDateTime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn drives_model_loop_against_mock() {
    let srv = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![
            roki_api_types::TicketSummary {
                ticket_id: "ENG-1".into(),
                repo: "github.com/x/y".into(),
                status: "in_progress".into(),
                labels: vec!["urgent".into()],
                assignee: "u".into(),
                in_flight_cycle_id: None,
                last_event_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            },
        ]))
        .mount(&srv)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(roki_api_types::EventsPage {
            events: vec![],
            gap: false,
            next_since: None,
        }))
        .mount(&srv)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/escalations"))
        .respond_with(ResponseTemplate::new(200).set_body_json::<Vec<roki_api_types::ApiEscalation>>(vec![]))
        .mount(&srv)
        .await;

    let model = App::run_for_test(&srv.uri(), 3, Duration::from_secs(6))
        .await
        .expect("run_for_test");
    assert!(!model.tickets.rows.is_empty(), "tickets snapshot must arrive");
    assert_eq!(model.tickets.rows[0].ticket_id, "ENG-1");
}
```

- [ ] **Step 2: `tests/tui_refresh_ack.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;

use roki_tui::client::ApiClient;
use roki_tui::model::{AppModel, Update};
use roki_tui::palette::Palette;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn post_refresh_ack_lands_in_status() {
    let srv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/refresh"))
        .respond_with(ResponseTemplate::new(202).set_body_json(roki_api_types::RefreshAck {
            coalesced: false,
            earliest_fire_at: None,
            backoff_active: false,
        }))
        .mount(&srv)
        .await;

    let client = Arc::new(ApiClient::new(&srv.uri()).unwrap());
    let (tx, mut rx) = mpsc::channel::<Update>(8);
    let c = client.clone();
    tokio::spawn(async move {
        let ack = c.post_refresh().await.unwrap();
        tx.send(Update::RefreshAck(ack)).await.unwrap();
    });
    let update = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let mut model = AppModel::new(Palette::IndexedAnsi16);
    match update {
        Update::RefreshAck(ack) => model.apply_refresh_ack(ack),
        other => panic!("unexpected update: {other:?}"),
    }
    assert!(model.status.text().contains("coalesced=false"));
    assert!(model.status.text().contains("backoff_active=false"));
}
```

- [ ] **Step 3: `tests/tui_palette_fallback.rs`**

```rust
use roki_tui::palette::{detect_with, EnvProbe, Palette};

struct DumbEnv;
impl EnvProbe for DumbEnv {
    fn get(&self, key: &str) -> Option<String> {
        if key == "TERM" { Some("dumb".into()) } else { None }
    }
}

#[test]
fn dumb_terminal_falls_back_to_16() {
    assert_eq!(detect_with(&DumbEnv), Palette::IndexedAnsi16);
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p roki-tui --tests`
Expected: every unit + integration test PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-tui/tests/
git commit -m "test(slice10,tui): smoke + refresh_ack + palette fallback"
```

**Acceptance:** all roki-tui tests pass.

---

## Task 17: `ref:cli` update

**Files:**
- Modify: `docs/reference/cli.md`

- [ ] **Step 1: Add subcommand row**

Open `docs/reference/cli.md`. In the `## Subcommands` table, after the `roki workflow graph` row append:

```markdown
| `roki-tui` | Terminal UI client for the observability HTTP API | [fr:11-roki-tui](../fr/11-roki-tui.md) |
```

- [ ] **Step 2: Add `## roki-tui` section**

After the existing `## roki workflow graph` section and before `## When adding a new flag`, insert:

```markdown
## `roki-tui`

Standalone `ratatui` binary. Connects to the observability HTTP API and renders four views.

| Flag | Argument | Overrides | Purpose |
|---|---|---|---|
| (positional) | `API_URL` | (none) | Base URL of the API (http or https). Required. |
| `--config <path>` | path | (none) | Override `~/.config/roki-tui/config.toml`. Required to exist when supplied. |
| `--tickets-cadence <secs>` | int | `[polling].tickets_seconds` | Tickets refresh cadence (min 1). |
| `--events-cadence <secs>` | int | `[polling].events_seconds` | Events tail cadence (min 1). |
| `--escalations-cadence <secs>` | int | `[polling].escalations_seconds` | Escalations refresh cadence (min 1). |

Configuration file schema:

```toml
[polling]
tickets_seconds = 2       # default 2, min 1
events_seconds = 1        # default 1, min 1
escalations_seconds = 5   # default 5, min 1
```
```

- [ ] **Step 3: Validate**

Run: `kusara validate`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add docs/reference/cli.md
git commit -m "docs(slice10,ref:cli): add roki-tui section"
```

**Acceptance:** `ref:cli` lists the new subcommand; kusara validate clean.

---

## Task 18: final sweep

**Files:** every touched file.

- [ ] **Step 1: rustfmt**

Run: `cargo fmt --all`
Expected: no changes (or fix and recommit).

- [ ] **Step 2: clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 3: full test suite**

Run: `cargo test --workspace`
Expected: every test PASSes.

- [ ] **Step 4: kusara validate**

Run: `kusara validate`
Expected: clean.

- [ ] **Step 5: Commit any fmt/clippy fixes (if needed)**

```bash
git add -A
git commit -m "chore(slice10): rustfmt + clippy clean"
```

(Skip the commit if nothing changed.)

- [ ] **Step 6: Verify branch state**

Run: `git log --oneline feature/slice10-roki-tui ^main | head -30`
Expected: roughly 18 commits in topic-tagged sequence.

**Acceptance:** workspace builds, tests pass, clippy clean, kusara clean, branch ready for PR.

---

## Self-Review Checklist (run after writing — informational)

1. **Spec coverage:**
   - `fr:11 §Startup and connection` → Task 5 (CLI), Task 9 (client), Task 11 (poll), Task 15 (drive loop).
   - `fr:11 §Configuration` → Task 6 (config loader); CLI overrides in Task 5.
   - `fr:11 §Views` → Task 12 (widgets); model in Task 10.
   - `fr:11 §Escalation acknowledgement` → Task 10 (`AckKey` + `apply` reconcile), Task 12 (glyph), Task 13 (`Action::ToggleAck`).
   - `fr:11 §Refresh action` → Task 15 (`Action::Refresh` + `RefreshState`), Task 9 (`post_refresh`).
   - `fr:11 §Log inspection` → Task 13 (`Action::PrintLogCmd`), Task 15 (status-bar copy).
   - `fr:11 §Terminal compatibility` → Task 8 (palette), Task 15 (Windows exit).
   - `fr:11 §Defense-in-depth sanitization` → Task 7 (sanitize), Task 9 (client integration).
   - `fr:11 §Shared types` → Task 9 (only `roki-api-types`).
   - `fr:11 §Logging` → Task 14 (startup log).
   - Design §3 `last_state_id` → Tasks 1–3.
   - Design §11 spec impact → Task 17 (ref:cli).

2. **Placeholder scan:** none — every step has executable code or commands.

3. **Type consistency:** `ApiClient` constructor returns `Result<Self, ClientError>` across tasks; `Update` enum spelled identically in model + poll + app; `AckKey` derives `Hash` (escalations test depends on it); `Palette` enum stable across tasks 8/14/15; `RefreshState` defined in `model::status` and used in `model::mod` + `app`.
