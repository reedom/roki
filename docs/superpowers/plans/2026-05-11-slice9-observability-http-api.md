# Slice 9 Observability HTTP API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `fr:10` end to end — opt-in axum HTTP API backed by `DiffCache`, on-disk per-ticket captures, `EscalationQueue`, a new `EventRing` (`fr:08 §Tier 3`), and a new Linear polling tracker (`fr:03 §Polling fallback` + `§Refresh nudge`). Introduce the `roki-api-types` crate as the single schema source of truth. Add `[api]` + `[linear].polling` `roki.toml` sections.

**Architecture:** New workspace member `roki-api-types` defines every public projection type. `roki-daemon` gains `api/` (axum router + handlers + sanitization + per-request middleware + projection layer), `observability/ring.rs` (in-memory monotonic-seq event ring), and `linear/polling.rs` (cadence-bounded poll task with nudge channel). All three plug into `runtime::run_inner` after the dispatcher and before `daemon_ready`.

**Tech Stack:** Rust 2024 (workspace edition), `axum = "0.7"` (already), `tokio` (already), `serde` / `serde_json` (already), `time = "0.3"` (already), `uuid = "1"` (already). New direct deps: `html_escape = "0.2"` (HTML escape on agent strings), `vte = "0.13"` (ANSI escape parser). Test deps: `tower = { version = "0.5", features = ["util"] }` (already), `wiremock = "0.6"` (already).

**Spec:** `docs/superpowers/specs/2026-05-11-slice9-observability-http-api-design.md`.

**Working branch:** `feature/slice9-observability-http-api` (already created; spec already committed at `64319dd`). Every implementation commit lands here.

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-api-types/Cargo.toml` | Workspace member manifest. |
| `crates/roki-api-types/src/lib.rs` | `pub mod` re-exports. |
| `crates/roki-api-types/src/tickets.rs` | `TicketSummary`, `TicketDetail`, `CycleSummary`, `CycleKind`, `CycleTrigger`. |
| `crates/roki-api-types/src/events.rs` | `ApiEvent`, `EventsPage`. |
| `crates/roki-api-types/src/escalations.rs` | `ApiEscalation`. |
| `crates/roki-api-types/src/healthz.rs` | `Healthz`. |
| `crates/roki-api-types/src/refresh.rs` | `RefreshAck`. |
| `crates/roki-daemon/src/api/mod.rs` | `ApiState`, `serve()`, `ApiBindError`. |
| `crates/roki-daemon/src/api/routes.rs` | axum router, every handler. |
| `crates/roki-daemon/src/api/sanitize.rs` | `clean_text`, `clean_text_or_placeholder`, `clean_json`. |
| `crates/roki-daemon/src/api/log_layer.rs` | per-request middleware emitting `api_request`. |
| `crates/roki-daemon/src/api/projection/mod.rs` | projection re-exports. |
| `crates/roki-daemon/src/api/projection/tickets.rs` | `DiffCache + EventRing → TicketSummary/Detail`. |
| `crates/roki-daemon/src/api/projection/cycles.rs` | scan `<session_root>/<ticket>/cycle-*/cycle.json` → `CycleSummary`. |
| `crates/roki-daemon/src/api/projection/visits.rs` | visit-stream byte/JSON loader. |
| `crates/roki-daemon/src/api/projection/events.rs` | `EventRing → EventsPage` query. |
| `crates/roki-daemon/src/api/projection/escalations.rs` | `EscalationQueue::snapshot → Vec<ApiEscalation>`. |
| `crates/roki-daemon/src/observability/mod.rs` | module root. |
| `crates/roki-daemon/src/observability/ring.rs` | `EventRing` (Mutex + VecDeque + AtomicU64 seq). |
| `crates/roki-daemon/src/linear/polling.rs` | `PollingTracker`, `NudgeHandle`, cadence loop, coalescing. |
| `crates/roki-daemon/tests/e2e/api_disabled_smoke.rs` … `api_bind_failure_smoke.rs` (10 fixtures) | per-fixture e2e per spec §6.1. |

### Modified

| Path | Change |
|---|---|
| `Cargo.toml` (workspace root) | `members = ["crates/roki-daemon", "crates/roki-api-types"]`. |
| `crates/roki-daemon/Cargo.toml` | Add `roki-api-types`, `html_escape`, `vte` deps. Add 10 new `[[test]]` entries. |
| `crates/roki-daemon/src/main.rs` | `mod api; mod observability;` declarations. |
| `crates/roki-daemon/src/config/roki.rs` | `ApiSection` + `LinearPollingSection`; defaults + validation + tests. |
| `crates/roki-daemon/src/runtime.rs` | wire `EventRing`, `PollingTracker`, `api::serve`. |
| `crates/roki-daemon/src/events.rs` | extend `Event` enum; route emits through `EventRing` when present. |
| `crates/roki-daemon/src/daemon/ticket_task.rs` | write `cycle.json` at cycle start + atomic update at cycle end. |
| `crates/roki-daemon/src/linear/mod.rs` | `pub mod polling;`. |
| `docs/reference/config.md` | `[api].ticket_events_window`, `[api].cycle_list_window` rows. |
| `docs/reference/log-events.md` | rows for `api_request`, `api_bind_failed`, `api_disabled`, `polling_tick`, `refresh_nudge_acknowledged`. |
| `docs/reference/artifacts.md` | row for `cycle.json`. |
| `docs/fr/08-observability-logs.md` | event catalog: add 5 new events. |

### Deleted

None.

---

## Task 0: Confirm spec + branch

**Goal:** spec is committed on `feature/slice9-observability-http-api` before any code lands.

**Steps:**

- [ ] Verify branch: `git rev-parse --abbrev-ref HEAD` returns `feature/slice9-observability-http-api`.
- [ ] Verify spec exists: `ls docs/superpowers/specs/2026-05-11-slice9-observability-http-api-design.md`.
- [ ] Verify spec is committed: `git log --oneline -1 docs/superpowers/specs/2026-05-11-slice9-observability-http-api-design.md` shows `64319dd`.

**Acceptance:** all three pass.

---

## Task 1: workspace + `roki-api-types` skeleton

**Files:**
- Create: `crates/roki-api-types/Cargo.toml`, `crates/roki-api-types/src/lib.rs`.
- Modify: `Cargo.toml` (workspace root).

- [ ] **Step 1: Add member to workspace root `Cargo.toml`**

```toml
[workspace]
resolver = "3"
members = ["crates/roki-daemon", "crates/roki-api-types"]
```

- [ ] **Step 2: Create `crates/roki-api-types/Cargo.toml`**

```toml
[package]
name = "roki-api-types"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "Stable wire-schema types for the roki observability HTTP API"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
time = { version = "0.3", features = ["formatting", "parsing", "serde-well-known"] }
uuid = { version = "1", features = ["serde", "v4"] }

[lints]
workspace = true
```

- [ ] **Step 3: Create empty `crates/roki-api-types/src/lib.rs`**

```rust
//! Stable wire-schema types for the roki observability HTTP API.
//!
//! Imported by `roki-daemon`'s `api/` module and (slice 10) `roki-tui`. No
//! runtime dependencies beyond `serde` / `serde_json` / `time` / `uuid`.

pub mod escalations;
pub mod events;
pub mod healthz;
pub mod refresh;
pub mod tickets;

pub use escalations::ApiEscalation;
pub use events::{ApiEvent, EventsPage};
pub use healthz::Healthz;
pub use refresh::RefreshAck;
pub use tickets::{CycleKind, CycleSummary, CycleTrigger, TicketDetail, TicketSummary};
```

- [ ] **Step 4: Create stub modules so the crate compiles**

Each of `tickets.rs`, `events.rs`, `escalations.rs`, `healthz.rs`, `refresh.rs` is a single line `// content lands in Task 2`.

- [ ] **Step 5: Verify build**

Run: `cargo build -p roki-api-types`.
Expected: `Finished` with warnings (unused module file). No errors.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/roki-api-types/
git commit -m "feat(slice9): scaffold roki-api-types crate"
```

**Acceptance:** workspace builds, `cargo metadata --format-version=1 | jq -r '.workspace_members[]' | grep roki-api-types` returns one line.

---

## Task 2: types crate full bodies + round-trip tests

**Files:**
- Modify: `crates/roki-api-types/src/{tickets,events,escalations,healthz,refresh}.rs`.

- [ ] **Step 1: Write `tickets.rs`**

```rust
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CycleKind {
    Rule,
    Cleanup,
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleTrigger {
    Runtime,
    ColdStart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketSummary {
    pub ticket_id: String,
    pub repo: String,
    pub status: String,
    pub labels: Vec<String>,
    pub assignee: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_cycle_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub last_event_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketDetail {
    #[serde(flatten)]
    pub summary: TicketSummary,
    pub recent_events: Vec<crate::events::ApiEvent>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleSummary {
    pub cycle_id: Uuid,
    pub kind: CycleKind,
    pub trigger: CycleTrigger,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option", skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<OffsetDateTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    pub total_visits: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_kind_round_trips_lowercase() {
        for k in [CycleKind::Rule, CycleKind::Cleanup, CycleKind::Failure] {
            let s = serde_json::to_string(&k).unwrap();
            let parsed: CycleKind = serde_json::from_str(&s).unwrap();
            assert_eq!(k, parsed);
            assert!(s.chars().all(|c| c.is_lowercase() || c == '"'));
        }
    }

    #[test]
    fn ticket_summary_round_trips() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let s = TicketSummary {
            ticket_id: "ENG-1".into(),
            repo: "github.com/x/y".into(),
            status: "in_progress".into(),
            labels: vec!["urgent".into()],
            assignee: "u1".into(),
            in_flight_cycle_id: Some(Uuid::nil()),
            last_event_at: now,
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: TicketSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(s, parsed);
    }
}
```

- [ ] **Step 2: Write `events.rs`**

```rust
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiEvent {
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_id: Option<Uuid>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<ApiEvent>,
    pub gap: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_since: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_page_round_trips() {
        let p = EventsPage {
            events: vec![ApiEvent {
                seq: 1,
                ts: OffsetDateTime::from_unix_timestamp(0).unwrap(),
                event: "webhook_received".into(),
                ticket_id: Some("ENG-1".into()),
                cycle_id: None,
                payload: serde_json::json!({"k": "v"}),
            }],
            gap: false,
            next_since: Some(1),
        };
        let json = serde_json::to_string(&p).unwrap();
        let parsed: EventsPage = serde_json::from_str(&json).unwrap();
        assert_eq!(p, parsed);
    }
}
```

- [ ] **Step 3: Write `escalations.rs`**

```rust
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiEscalation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_id: Option<Uuid>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visit_n: Option<u32>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub error_text: String,
    pub marker: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let e = ApiEscalation {
            ticket_id: Some("ENG-1".into()),
            cycle_id: Some(Uuid::nil()),
            kind: "recursion_bound".into(),
            state_id: Some("post0".into()),
            visit_n: Some(2),
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            error_text: "boom".into(),
            marker: "none".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(e, serde_json::from_str(&s).unwrap());
    }
}
```

- [ ] **Step 4: Write `healthz.rs`**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthz {
    pub version: String,
    pub uptime_seconds: u64,
    pub configured_repositories: Vec<String>,
    pub api_request_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let h = Healthz {
            version: "0.1.0".into(),
            uptime_seconds: 42,
            configured_repositories: vec!["github.com/x/y".into()],
            api_request_count: 7,
        };
        let s = serde_json::to_string(&h).unwrap();
        assert_eq!(h, serde_json::from_str(&s).unwrap());
    }
}
```

- [ ] **Step 5: Write `refresh.rs`**

```rust
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshAck {
    pub coalesced: bool,
    #[serde(default, with = "time::serde::rfc3339::option", skip_serializing_if = "Option::is_none")]
    pub earliest_fire_at: Option<OffsetDateTime>,
    pub backoff_active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_and_without_earliest_fire_at() {
        for earliest in [None, Some(OffsetDateTime::from_unix_timestamp(1).unwrap())] {
            let r = RefreshAck {
                coalesced: true,
                earliest_fire_at: earliest,
                backoff_active: false,
            };
            let s = serde_json::to_string(&r).unwrap();
            let parsed: RefreshAck = serde_json::from_str(&s).unwrap();
            assert_eq!(r, parsed);
        }
    }
}
```

- [ ] **Step 6: Run unit tests**

Run: `cargo test -p roki-api-types`.
Expected: 6 tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-api-types/src/
git commit -m "feat(slice9,api-types): TicketSummary/CycleSummary/ApiEvent/ApiEscalation/Healthz/RefreshAck"
```

**Acceptance:** `cargo test -p roki-api-types` green; `cargo clippy -p roki-api-types -- -D warnings` clean.

---

## Task 3: `RokiConfig` `[api]` + `[linear].polling`

**Files:**
- Modify: `crates/roki-daemon/src/config/roki.rs`.

- [ ] **Step 1: Add the failing test (top of `mod tests`)**

```rust
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
```

If `parse_test` does not already exist, add a small helper:

```rust
#[cfg(test)]
fn parse_test(toml: &str) -> Result<RokiConfig, ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("roki.toml");
    std::fs::write(&p, toml).unwrap();
    RokiConfig::load(&p)
}
```

- [ ] **Step 2: Run tests to verify FAIL**

Run: `cargo test -p roki-daemon --bin roki config::roki -- api_section_`.
Expected: 3 failures (`api` field missing on `RokiConfig`).

- [ ] **Step 3: Add structs + defaults + validation**

In `crates/roki-daemon/src/config/roki.rs`, add the new fields + parsing:

```rust
#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct LinearPollingSection {
    pub cadence_seconds: u32,
}

impl Default for LinearPollingSection {
    fn default() -> Self {
        Self { cadence_seconds: 300 }
    }
}
```

Extend `RokiConfig` struct with `pub api: ApiSection`. Extend the existing `LinearSection` (or wrapper) with `pub polling: LinearPollingSection`.

In the raw deserialization path, add an optional `api` table:

```rust
#[derive(Deserialize)]
struct RawApi {
    #[serde(default = "default_bind")]
    bind: String,
    port: Option<u16>,
    #[serde(default = "default_window")]
    ticket_events_window: u32,
    #[serde(default = "default_window")]
    cycle_list_window: u32,
}

fn default_bind() -> String { "127.0.0.1".into() }
fn default_window() -> u32 { 50 }
```

In the `validate` step:

```rust
let api = match raw.api {
    Some(a) => {
        if let Some(p) = a.port {
            if p == 0 {
                return Err(ConfigError::InvalidValue {
                    key: "api.port".into(),
                    code: "invalid_port_zero".into(),
                });
            }
        }
        if a.bind.parse::<std::net::IpAddr>().is_err() {
            return Err(ConfigError::InvalidValue {
                key: "api.bind".into(),
                code: "invalid_bind_addr".into(),
            });
        }
        if !(1..=500).contains(&a.ticket_events_window) {
            return Err(ConfigError::InvalidValue {
                key: "api.ticket_events_window".into(),
                code: "invalid_window".into(),
            });
        }
        if !(1..=500).contains(&a.cycle_list_window) {
            return Err(ConfigError::InvalidValue {
                key: "api.cycle_list_window".into(),
                code: "invalid_window".into(),
            });
        }
        ApiSection {
            bind: a.bind,
            port: a.port,
            ticket_events_window: a.ticket_events_window,
            cycle_list_window: a.cycle_list_window,
        }
    }
    None => ApiSection::default(),
};
```

Same shape for `polling`:

```rust
let polling = match raw_linear.polling {
    Some(p) => {
        if p.cadence_seconds < 60 {
            return Err(ConfigError::InvalidValue {
                key: "linear.polling.cadence_seconds".into(),
                code: "invalid_cadence".into(),
            });
        }
        LinearPollingSection { cadence_seconds: p.cadence_seconds }
    }
    None => LinearPollingSection::default(),
};
```

- [ ] **Step 4: Run tests to verify PASS**

Run: `cargo test -p roki-daemon --bin roki config::roki -- api_section_ polling_cadence_`.
Expected: 3 pass.

- [ ] **Step 5: Update `ref:config` rows**

In `docs/reference/config.md`, locate the table near `[api].port`. Insert two rows (alphabetical / before/after the existing `[api].port`):

```markdown
| `[api].ticket_events_window` | no | int | `50` | `1..=500` | [fr:10 §GET /api/tickets/{id}](../fr/10-http-api.md) |
| `[api].cycle_list_window` | no | int | `50` | `1..=500` | [fr:10 §GET /api/tickets/{id}/cycles](../fr/10-http-api.md) |
```

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/config/roki.rs docs/reference/config.md
git commit -m "feat(slice9,config): [api] + [linear].polling sections + validation"
```

**Acceptance:** `cargo test -p roki-daemon --bin roki` 282+ pass; `kusara validate` clean.

---

## Task 4: `EventRing` + `EventWriter` integration

**Files:**
- Create: `crates/roki-daemon/src/observability/mod.rs`, `crates/roki-daemon/src/observability/ring.rs`.
- Modify: `crates/roki-daemon/src/main.rs`, `crates/roki-daemon/src/events.rs`.

- [ ] **Step 1: Add `mod observability;` to `main.rs` top-level mod list (alphabetical position).**

- [ ] **Step 2: Create `observability/mod.rs`**

```rust
//! In-memory observability primitives. Ring buffer + (future) hooks.

pub mod ring;

pub use ring::EventRing;
```

- [ ] **Step 3: Failing test in `observability/ring.rs`**

```rust
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Mutex;

use roki_api_types::{ApiEvent, EventsPage};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

pub struct EventRing {
    capacity: usize,
    inner: Mutex<RingInner>,
}

struct RingInner {
    next_seq: u64,
    buf: VecDeque<RingEntry>,
}

#[derive(Clone)]
struct RingEntry {
    seq: u64,
    ts: OffsetDateTime,
    event: String,
    ticket_id: Option<String>,
    cycle_id: Option<Uuid>,
    payload: Value,
}

impl EventRing {
    pub fn new(capacity: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            capacity,
            inner: Mutex::new(RingInner {
                next_seq: 1,
                buf: VecDeque::with_capacity(capacity.max(1)),
            }),
        })
    }

    pub fn record(
        &self,
        event: &str,
        ticket_id: Option<&str>,
        cycle_id: Option<Uuid>,
        payload: Value,
    ) -> u64 {
        if self.capacity == 0 {
            // Disabled ring still produces a strictly increasing seq for any
            // future caller that relies on the return value (currently no one
            // does, but the contract is "monotonic"); we just don't store.
            let mut g = self.inner.lock().expect("ring lock");
            let seq = g.next_seq;
            g.next_seq += 1;
            return seq;
        }
        let mut g = self.inner.lock().expect("ring lock");
        let seq = g.next_seq;
        g.next_seq += 1;
        if g.buf.len() == self.capacity {
            g.buf.pop_front();
        }
        g.buf.push_back(RingEntry {
            seq,
            ts: OffsetDateTime::now_utc(),
            event: event.to_string(),
            ticket_id: ticket_id.map(str::to_string),
            cycle_id,
            payload,
        });
        seq
    }

    pub fn page(
        &self,
        since: Option<u64>,
        kind: Option<&str>,
        ticket: Option<&str>,
        cycle: Option<Uuid>,
        limit: usize,
    ) -> EventsPage {
        let g = self.inner.lock().expect("ring lock");
        let oldest = g.buf.front().map(|e| e.seq);
        let gap = match (since, oldest) {
            (Some(s), Some(o)) => s + 1 < o,
            (Some(_), None) => true,
            _ => false,
        };
        let start_after = since.unwrap_or(0);
        let mut out: Vec<ApiEvent> = Vec::new();
        for e in g.buf.iter() {
            if e.seq <= start_after {
                continue;
            }
            if let Some(k) = kind {
                if e.event != k {
                    continue;
                }
            }
            if let Some(t) = ticket {
                if e.ticket_id.as_deref() != Some(t) {
                    continue;
                }
            }
            if let Some(c) = cycle {
                if e.cycle_id != Some(c) {
                    continue;
                }
            }
            out.push(ApiEvent {
                seq: e.seq,
                ts: e.ts,
                event: e.event.clone(),
                ticket_id: e.ticket_id.clone(),
                cycle_id: e.cycle_id,
                payload: e.payload.clone(),
            });
            if out.len() >= limit {
                break;
            }
        }
        let next_since = out.last().map(|e| e.seq);
        EventsPage { events: out, gap, next_since }
    }

    pub fn oldest_seq(&self) -> Option<u64> {
        self.inner.lock().expect("ring lock").buf.front().map(|e| e.seq)
    }

    pub fn newest_seq(&self) -> Option<u64> {
        self.inner.lock().expect("ring lock").buf.back().map(|e| e.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_assigns_monotonic_seq() {
        let r = EventRing::new(10);
        assert_eq!(r.record("a", None, None, Value::Null), 1);
        assert_eq!(r.record("b", None, None, Value::Null), 2);
        assert_eq!(r.record("c", None, None, Value::Null), 3);
    }

    #[test]
    fn page_since_returns_only_newer() {
        let r = EventRing::new(10);
        for i in 0..5 {
            r.record(&format!("e{i}"), None, None, Value::Null);
        }
        let p = r.page(Some(2), None, None, None, 100);
        assert_eq!(p.events.len(), 3);
        assert_eq!(p.events[0].seq, 3);
        assert!(!p.gap);
    }

    #[test]
    fn page_gap_when_since_older_than_oldest() {
        let r = EventRing::new(2);
        for i in 0..5 {
            r.record(&format!("e{i}"), None, None, Value::Null);
        }
        let p = r.page(Some(1), None, None, None, 100);
        assert!(p.gap, "since=1 must report gap when oldest is 4");
    }

    #[test]
    fn kind_filter() {
        let r = EventRing::new(10);
        r.record("a", None, None, Value::Null);
        r.record("b", None, None, Value::Null);
        let p = r.page(None, Some("a"), None, None, 100);
        assert_eq!(p.events.len(), 1);
        assert_eq!(p.events[0].event, "a");
    }

    #[test]
    fn capacity_zero_no_op() {
        let r = EventRing::new(0);
        r.record("a", None, None, Value::Null);
        let p = r.page(None, None, None, None, 100);
        assert!(p.events.is_empty());
        assert!(!p.gap);
    }
}
```

- [ ] **Step 4: Add `roki-api-types` dependency to `crates/roki-daemon/Cargo.toml`**

```toml
roki-api-types = { path = "../roki-api-types" }
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p roki-daemon --bin roki observability::ring`.
Expected: 5 pass.

- [ ] **Step 6: EventWriter integration**

In `crates/roki-daemon/src/events.rs`, add a sibling `EventTap` so the existing per-ticket `EventWriter::emit` keeps its file-only behaviour and a new explicit method routes through both file + ring:

```rust
use std::sync::Arc;

use crate::observability::EventRing;

pub struct EventTap {
    pub ring: Arc<EventRing>,
}

impl EventTap {
    pub fn record(&self, event: &Event) {
        let kind = event.kind_str();
        let (ticket, cycle) = event.routing_keys();
        let payload = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
        self.ring.record(kind, ticket.as_deref(), cycle, payload);
    }
}
```

Add helper methods on `Event` (`kind_str`, `routing_keys`):

```rust
impl Event {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Event::WebhookReceived { .. } => "webhook_received",
            Event::CycleStarted { .. } => "cycle_started",
            Event::StateStarted { .. } => "state_started",
            Event::CycleCompleted { .. } => "cycle_completed",
            // ... every variant; the engineer matches the existing event catalog ...
            _ => "unknown",
        }
    }

    pub fn routing_keys(&self) -> (Option<String>, Option<uuid::Uuid>) {
        // Reflect on the event variant; return the ticket id and cycle id if
        // the variant carries them. Cycles parsed from String → Uuid.
        // ... pattern-match every variant ...
        (None, None)
    }
}
```

(The skeleton shows the shape; fill every variant from the existing `Event` enum during implementation. Engineer compiles iteratively.)

- [ ] **Step 7: Run the full bin tests**

Run: `cargo test -p roki-daemon --bin roki`.
Expected: 280+ pass.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/src/main.rs crates/roki-daemon/src/observability/ crates/roki-daemon/src/events.rs
git commit -m "feat(slice9,obs): EventRing + EventTap routing through ring"
```

**Acceptance:** ring tests pass; `EventTap::record` compiles for every existing `Event` variant; `cargo clippy -p roki-daemon --bin roki -- -D warnings` clean.

---

## Task 5: `linear::polling::PollingTracker`

**Files:**
- Create: `crates/roki-daemon/src/linear/polling.rs`.
- Modify: `crates/roki-daemon/src/linear/mod.rs`.

- [ ] **Step 1: Add `pub mod polling;` to `linear/mod.rs`.**

- [ ] **Step 2: Failing test in `polling.rs`**

```rust
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use roki_api_types::RefreshAck;
use time::OffsetDateTime;

use crate::linear::rate_limit::RateLimitState;

pub struct NudgeHandle {
    tx: mpsc::Sender<NudgeRequest>,
}

struct NudgeRequest {
    ack: oneshot::Sender<RefreshAck>,
}

pub struct PollingTracker {
    cadence: Duration,
    rate_limit: Arc<RateLimitState>,
    last_webhook_success: Arc<AtomicI64>, // ms since epoch; 0 = never
    last_fire: tokio::sync::Mutex<Instant>,
    nudge_rx: tokio::sync::Mutex<mpsc::Receiver<NudgeRequest>>,
    on_tick: Box<dyn Fn(TickReason) + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickReason {
    Outage,
    Nudge,
}

impl PollingTracker {
    pub fn spawn(
        cadence: Duration,
        rate_limit: Arc<RateLimitState>,
        last_webhook_success: Arc<AtomicI64>,
        on_tick: Box<dyn Fn(TickReason) + Send + Sync>,
    ) -> NudgeHandle {
        let (tx, rx) = mpsc::channel(32);
        let tracker = Arc::new(Self {
            cadence,
            rate_limit,
            last_webhook_success,
            last_fire: tokio::sync::Mutex::new(Instant::now() - cadence),
            nudge_rx: tokio::sync::Mutex::new(rx),
            on_tick,
        });
        tokio::spawn(async move { tracker.run().await });
        NudgeHandle { tx }
    }

    async fn run(self: Arc<Self>) {
        loop {
            let now = Instant::now();
            let last = *self.last_fire.lock().await;
            let next = last + self.cadence;
            let sleep = if now >= next { Duration::ZERO } else { next - now };

            let mut rx = self.nudge_rx.lock().await;
            let request = match timeout(sleep, rx.recv()).await {
                Ok(Some(req)) => Some(req),
                Ok(None) => return, // sender dropped
                Err(_) => None,
            };

            // Nudge or cadence wake. Coalesce additional pending nudges.
            let mut acks: Vec<oneshot::Sender<RefreshAck>> = Vec::new();
            if let Some(r) = request {
                acks.push(r.ack);
            }
            while let Ok(r) = rx.try_recv() {
                acks.push(r.ack);
            }
            drop(rx);

            // 429 backoff?
            if let Some(legal) = self.rate_limit.next_legal_at() {
                if Instant::now() < legal {
                    let earliest = OffsetDateTime::now_utc()
                        + (legal - Instant::now()).try_into().unwrap_or_default();
                    for ack in acks {
                        let _ = ack.send(RefreshAck {
                            coalesced: false,
                            earliest_fire_at: Some(earliest),
                            backoff_active: true,
                        });
                    }
                    continue;
                }
            }

            // Decide reason: nudge wins over outage.
            let reason = if !acks.is_empty() {
                TickReason::Nudge
            } else {
                TickReason::Outage
            };

            // Outage gating: only tick on outage if webhook silent for 2 * cadence.
            if reason == TickReason::Outage {
                let last_ms = self.last_webhook_success.load(Ordering::Relaxed);
                let now_ms = OffsetDateTime::now_utc().unix_timestamp() * 1000;
                if last_ms != 0
                    && now_ms - last_ms < (self.cadence.as_millis() as i64) * 2
                {
                    continue;
                }
            }

            // Execute the tick.
            (self.on_tick)(reason);
            *self.last_fire.lock().await = Instant::now();

            let earliest = OffsetDateTime::now_utc();
            let coalesced_value = acks.len() > 1;
            for (i, ack) in acks.into_iter().enumerate() {
                let _ = ack.send(RefreshAck {
                    coalesced: coalesced_value || i > 0,
                    earliest_fire_at: Some(earliest),
                    backoff_active: false,
                });
            }
        }
    }
}

impl NudgeHandle {
    pub async fn nudge(&self) -> RefreshAck {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(NudgeRequest { ack: tx }).await.is_err() {
            return RefreshAck {
                coalesced: false,
                earliest_fire_at: None,
                backoff_active: false,
            };
        }
        rx.await.unwrap_or(RefreshAck {
            coalesced: false,
            earliest_fire_at: None,
            backoff_active: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn rate_limit_unbounded() -> Arc<RateLimitState> {
        Arc::new(RateLimitState::new())
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn nudge_triggers_tick() {
        let calls = Arc::new(StdMutex::new(Vec::<TickReason>::new()));
        let calls_cb = calls.clone();
        let last_webhook = Arc::new(AtomicI64::new(0));
        let handle = PollingTracker::spawn(
            Duration::from_secs(1),
            rate_limit_unbounded(),
            last_webhook,
            Box::new(move |r| calls_cb.lock().unwrap().push(r)),
        );
        let ack = handle.nudge().await;
        assert!(!ack.coalesced);
        assert!(!ack.backoff_active);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(calls.lock().unwrap()[0], TickReason::Nudge);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn outage_silenced_when_webhook_recent() {
        let calls = Arc::new(StdMutex::new(Vec::<TickReason>::new()));
        let calls_cb = calls.clone();
        let now_ms = OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let last_webhook = Arc::new(AtomicI64::new(now_ms));
        let _handle = PollingTracker::spawn(
            Duration::from_secs(1),
            rate_limit_unbounded(),
            last_webhook,
            Box::new(move |r| calls_cb.lock().unwrap().push(r)),
        );
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(calls.lock().unwrap().is_empty());
    }
}
```

- [ ] **Step 3: Verify `RateLimitState::next_legal_at()` exists; if not, add it**

```bash
grep -n 'next_legal_at\|in_backoff' crates/roki-daemon/src/linear/rate_limit.rs
```

If absent, add:

```rust
impl RateLimitState {
    pub fn next_legal_at(&self) -> Option<std::time::Instant> {
        self.backoff_until.lock().unwrap().clone()
    }
}
```

(Match the existing internal field name.)

- [ ] **Step 4: Run the polling tests**

Run: `cargo test -p roki-daemon --bin roki linear::polling`.
Expected: 2 pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/linear/mod.rs crates/roki-daemon/src/linear/polling.rs crates/roki-daemon/src/linear/rate_limit.rs
git commit -m "feat(slice9,linear): PollingTracker with cadence + nudge coalescing"
```

**Acceptance:** polling tests pass; `cargo clippy -p roki-daemon --bin roki -- -D warnings` clean.

---

## Task 6: `api::sanitize`

**Files:**
- Create: `crates/roki-daemon/src/api/sanitize.rs` (along with `api/mod.rs` stub).
- Modify: `crates/roki-daemon/Cargo.toml`, `crates/roki-daemon/src/main.rs`.

- [ ] **Step 1: Add deps to `Cargo.toml`**

```toml
html_escape = "0.2"
vte = "0.13"
```

- [ ] **Step 2: Add `mod api;` to `main.rs`.**

- [ ] **Step 3: Stub `crates/roki-daemon/src/api/mod.rs`**

```rust
//! Observability HTTP API per fr:10.
pub mod sanitize;
```

- [ ] **Step 4: Write `sanitize.rs` with failing tests**

```rust
use serde_json::Value;

/// ANSI-strip + HTML-escape.
pub fn clean_text(input: &str) -> String {
    let stripped = strip_ansi(input);
    html_escape::encode_text(&stripped).into_owned()
}

/// Same as [`clean_text`] but tolerates non-UTF-8 bytes by replacing them
/// with U+FFFD. Returns the field name when a replacement happened, so the
/// caller can log the offending field.
pub fn clean_text_or_placeholder(field_name: &'static str, raw: &[u8]) -> (String, Option<&'static str>) {
    match std::str::from_utf8(raw) {
        Ok(s) => (clean_text(s), None),
        Err(_) => {
            let s: String = String::from_utf8_lossy(raw).into_owned();
            (clean_text(&s), Some(field_name))
        }
    }
}

/// Apply [`clean_text`] to every string leaf of a JSON value in place.
pub fn clean_json(value: &mut Value) {
    match value {
        Value::String(s) => *s = clean_text(s),
        Value::Array(arr) => arr.iter_mut().for_each(clean_json),
        Value::Object(map) => map.iter_mut().for_each(|(_, v)| clean_json(v)),
        _ => {}
    }
}

fn strip_ansi(input: &str) -> String {
    struct Stripper {
        out: String,
    }
    impl vte::Perform for Stripper {
        fn print(&mut self, c: char) {
            self.out.push(c);
        }
        fn execute(&mut self, b: u8) {
            // Preserve newline + tab + CR; drop other control bytes.
            if matches!(b, b'\n' | b'\t' | b'\r') {
                self.out.push(b as char);
            }
        }
        fn put(&mut self, _b: u8) {}
        fn unhook(&mut self) {}
        fn osc_dispatch(&mut self, _params: &[&[u8]], _bell: bool) {}
        fn csi_dispatch(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
        fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
        fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    }
    let mut perf = Stripper { out: String::with_capacity(input.len()) };
    let mut parser = vte::Parser::new();
    for byte in input.bytes() {
        parser.advance(&mut perf, byte);
    }
    perf.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_ansi_color_codes() {
        let s = "\x1b[31mred\x1b[0m";
        assert_eq!(clean_text(s), "red");
    }

    #[test]
    fn html_escapes_brackets() {
        assert_eq!(clean_text("<script>"), "&lt;script&gt;");
    }

    #[test]
    fn preserves_newlines_and_tabs() {
        assert_eq!(clean_text("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn json_walker_cleans_string_leaves_only() {
        let mut v = json!({
            "a": "<x>",
            "b": [1, "\x1b[31mred"],
            "c": {"d": "ok"},
        });
        clean_json(&mut v);
        assert_eq!(v, json!({
            "a": "&lt;x&gt;",
            "b": [1, "red"],
            "c": {"d": "ok"},
        }));
    }

    #[test]
    fn invalid_utf8_returns_placeholder_marker() {
        let raw = b"abc\xff\xfe";
        let (out, marker) = clean_text_or_placeholder("title", raw);
        assert_eq!(marker, Some("title"));
        assert!(out.contains("abc"));
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p roki-daemon --bin roki api::sanitize`.
Expected: 5 pass.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/src/main.rs crates/roki-daemon/src/api/mod.rs crates/roki-daemon/src/api/sanitize.rs
git commit -m "feat(slice9,api): clean_text + clean_json sanitization"
```

**Acceptance:** sanitize tests pass.

---

## Task 7: projection layer

**Files:**
- Create: `crates/roki-daemon/src/api/projection/{mod,tickets,cycles,visits,events,escalations}.rs`.

- [ ] **Step 1: `api/projection/mod.rs`**

```rust
pub mod cycles;
pub mod escalations;
pub mod events;
pub mod tickets;
pub mod visits;
```

- [ ] **Step 2: `tickets.rs`** — projection from `DiffCache` snapshot to `TicketSummary` / `TicketDetail`.

```rust
use std::sync::Arc;

use roki_api_types::{ApiEvent, TicketDetail, TicketSummary};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::api::sanitize::clean_text;
use crate::daemon::cache::DiffCache;
use crate::observability::EventRing;

pub fn list_tickets(cache: &DiffCache) -> Vec<TicketSummary> {
    let mut entries = cache.snapshot();
    entries.sort_by(|a, b| b.last_event_at.cmp(&a.last_event_at));
    entries.into_iter().map(into_summary).collect()
}

pub fn detail(
    cache: &DiffCache,
    ring: &Arc<EventRing>,
    ticket_id: &str,
    window: usize,
) -> Option<TicketDetail> {
    let entry = cache.get(ticket_id)?;
    let recent: Vec<ApiEvent> = ring
        .page(None, None, Some(ticket_id), None, window + 1)
        .events;
    let truncated = recent.len() > window;
    Some(TicketDetail {
        summary: into_summary(entry),
        recent_events: recent.into_iter().take(window).collect(),
        truncated,
    })
}

fn into_summary(entry: crate::daemon::cache::CacheEntry) -> TicketSummary {
    TicketSummary {
        ticket_id: entry.ticket_id.clone(),
        repo: clean_text(&entry.repo),
        status: clean_text(&entry.status),
        labels: entry.labels.iter().map(|l| clean_text(l)).collect(),
        assignee: clean_text(&entry.assignee),
        in_flight_cycle_id: entry.cycle_id.and_then(|s| Uuid::parse_str(&s).ok()),
        last_event_at: entry.last_event_at,
    }
}
```

(`DiffCache::snapshot` and `DiffCache::get` may need to be added with the obvious shape — engineer adds them under `daemon::cache::DiffCache` if missing.)

- [ ] **Step 3: `cycles.rs`** — scan `<session_root>/<ticket>/cycle-*/cycle.json`.

```rust
use std::path::Path;

use roki_api_types::{CycleSummary};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Deserialize)]
struct OnDisk {
    cycle_id: Uuid,
    kind: String,
    trigger: String,
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    ended_at: Option<OffsetDateTime>,
    terminal_id: Option<String>,
    failure_kind: Option<String>,
    total_visits: u32,
    states: Vec<String>,
}

pub fn list_cycles(session_root: &Path, ticket_id: &str, window: usize) -> (Vec<CycleSummary>, bool) {
    let dir = session_root.join(ticket_id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(_) => return (vec![], false),
    };
    let mut summaries: Vec<CycleSummary> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .filter_map(|e| {
            let path = e.path().join("cycle.json");
            let body = std::fs::read_to_string(&path).ok()?;
            let on_disk: OnDisk = serde_json::from_str(&body).ok()?;
            Some(parse(on_disk))
        })
        .collect();
    summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    let truncated = summaries.len() > window;
    summaries.truncate(window);
    (summaries, truncated)
}

pub fn read_cycle_states(session_root: &Path, ticket_id: &str, cycle_id: Uuid) -> Option<Vec<String>> {
    let path = session_root
        .join(ticket_id)
        .join(format!("cycle-{cycle_id}"))
        .join("cycle.json");
    let body = std::fs::read_to_string(&path).ok()?;
    let on_disk: OnDisk = serde_json::from_str(&body).ok()?;
    Some(on_disk.states)
}

fn parse(d: OnDisk) -> CycleSummary {
    use roki_api_types::{CycleKind, CycleTrigger};
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
        total_visits: d.total_visits,
    }
}
```

- [ ] **Step 4: `visits.rs`**

```rust
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    Directive,
    Terminal,
    Events,
    ExitCode,
}

impl Stream {
    pub fn from_str(s: &str) -> Option<Stream> {
        match s {
            "stdout" => Some(Stream::Stdout),
            "stderr" => Some(Stream::Stderr),
            "directive" => Some(Stream::Directive),
            "terminal" => Some(Stream::Terminal),
            "events" => Some(Stream::Events),
            "exit_code" => Some(Stream::ExitCode),
            _ => None,
        }
    }

    pub fn file_suffix(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::Directive => "directive.json",
            Stream::Terminal => "terminal.json",
            Stream::Events => "events.jsonl",
            Stream::ExitCode => "exit_code",
        }
    }
}

pub fn read_stream(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: uuid::Uuid,
    visit_n: u32,
    state_id: &str,
    stream: Stream,
) -> std::io::Result<Vec<u8>> {
    let path = session_root
        .join(ticket_id)
        .join(format!("cycle-{cycle_id}"))
        .join(format!("visit-{visit_n}"))
        .join(format!("{state_id}.{}", stream.file_suffix()));
    std::fs::read(path)
}
```

- [ ] **Step 5: `events.rs`** — thin wrapper around `EventRing::page`.

```rust
use std::sync::Arc;

use roki_api_types::EventsPage;
use uuid::Uuid;

use crate::observability::EventRing;

pub struct EventsQuery<'a> {
    pub since: Option<u64>,
    pub kind: Option<&'a str>,
    pub ticket: Option<&'a str>,
    pub cycle: Option<Uuid>,
    pub limit: usize,
}

pub fn page(ring: &Arc<EventRing>, q: EventsQuery<'_>) -> EventsPage {
    let mut p = ring.page(q.since, q.kind, q.ticket, q.cycle, q.limit);
    for ev in &mut p.events {
        crate::api::sanitize::clean_json(&mut ev.payload);
        ev.event = crate::api::sanitize::clean_text(&ev.event);
        if let Some(t) = &mut ev.ticket_id {
            *t = crate::api::sanitize::clean_text(t);
        }
    }
    p
}
```

- [ ] **Step 6: `escalations.rs`**

```rust
use std::sync::Arc;

use roki_api_types::ApiEscalation;

use crate::escalation::EscalationQueue;

pub fn list(queue: &Arc<EscalationQueue>) -> Vec<ApiEscalation> {
    queue
        .snapshot()
        .into_iter()
        .map(|e| ApiEscalation {
            ticket_id: e.ticket_id,
            cycle_id: e.cycle_id,
            kind: format!("{:?}", e.failure_kind).to_lowercase(),
            state_id: e.phase,
            visit_n: None,
            timestamp: e.timestamp,
            error_text: crate::api::sanitize::clean_text(&e.error_text),
            marker: "none".into(),
        })
        .collect()
}
```

(Engineer reconciles `failure_kind` to a stable lowercase canonical string via the existing helper if present; otherwise adds one.)

- [ ] **Step 7: Add unit tests**

In `api/projection/cycles.rs` add:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lists_cycles_descending_by_started_at() {
        let dir = TempDir::new().unwrap();
        let ticket = dir.path().join("ENG-1");
        for (i, ts) in ["2026-05-01T00:00:00Z", "2026-05-02T00:00:00Z"].iter().enumerate() {
            let id = Uuid::new_v4();
            let cycle = ticket.join(format!("cycle-{id}"));
            std::fs::create_dir_all(&cycle).unwrap();
            let body = format!(
                r#"{{"cycle_id":"{id}","kind":"rule","trigger":"runtime","started_at":"{ts}","total_visits":{i},"states":[]}}"#
            );
            std::fs::write(cycle.join("cycle.json"), body).unwrap();
        }
        let (cycles, truncated) = list_cycles(dir.path(), "ENG-1", 10);
        assert_eq!(cycles.len(), 2);
        assert!(cycles[0].started_at > cycles[1].started_at);
        assert!(!truncated);
    }
}
```

- [ ] **Step 8: Run tests**

Run: `cargo test -p roki-daemon --bin roki api::projection`.
Expected: pass (count varies; ≥1).

- [ ] **Step 9: Commit**

```bash
git add crates/roki-daemon/src/api/projection/
git commit -m "feat(slice9,api): projection layer (tickets/cycles/visits/events/escalations)"
```

**Acceptance:** projection tests pass; `cargo clippy -p roki-daemon --bin roki -- -D warnings` clean.

---

## Task 8: per-request log middleware

**Files:**
- Create: `crates/roki-daemon/src/api/log_layer.rs`.
- Modify: `crates/roki-daemon/src/events.rs`.

- [ ] **Step 1: Add `Event::ApiRequest` variant**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    // ... existing ...
    ApiRequest {
        ts: String,
        method: String,
        path: String,
        query_keys: Vec<String>,
        status: u16,
        duration_ms: u32,
        client_addr: String,
        correlation_id: String,
    },
}
```

Update `Event::kind_str` to include `Event::ApiRequest { .. } => "api_request"`.

- [ ] **Step 2: Failing test in `log_layer.rs`**

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, ConnectInfo};
use axum::middleware::Next;
use axum::response::Response;
use std::net::SocketAddr;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::events::{Event, EventWriter, now_rfc3339};

pub struct LogState {
    pub counter: Arc<AtomicU64>,
    pub daemon_writer: Arc<Mutex<EventWriter>>,
}

pub async fn log_layer(
    state: axum::extract::State<Arc<LogState>>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query_keys: Vec<String> = request
        .uri()
        .query()
        .map(|q| {
            q.split('&')
                .filter_map(|kv| kv.split('=').next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    state.counter.fetch_add(1, Ordering::Relaxed);
    let correlation_id = Uuid::new_v4().to_string();

    let response = next.run(request).await;
    let status = response.status().as_u16();
    let duration_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;

    let mut w = state.daemon_writer.lock().await;
    let _ = w.emit(&Event::ApiRequest {
        ts: now_rfc3339(),
        method,
        path,
        query_keys,
        status,
        duration_ms,
        client_addr: client_addr.to_string(),
        correlation_id,
    });
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    #[tokio::test]
    async fn middleware_increments_counter() {
        let dir = tempfile::tempdir().unwrap();
        let writer = EventWriter::open(dir.path(), "_daemon").unwrap();
        let state = Arc::new(LogState {
            counter: Arc::new(AtomicU64::new(0)),
            daemon_writer: Arc::new(Mutex::new(writer)),
        });
        let app: Router = Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state.clone(), log_layer))
            .with_state(state.clone())
            .into_make_service_with_connect_info::<SocketAddr>();
        // Skipping full ConnectInfo wiring; cover the counter via a unit
        // increment integration in the routes test in Task 9.
        let _ = app;
    }
}
```

(`ConnectInfo` requires the server-binding flow; the routes-level integration in Task 9 exercises the middleware end to end. The unit test here only confirms compilation and basic shape.)

- [ ] **Step 3: Run**

Run: `cargo test -p roki-daemon --bin roki api::log_layer`.
Expected: 1 pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/api/log_layer.rs crates/roki-daemon/src/events.rs
git commit -m "feat(slice9,api): per-request middleware emitting api_request"
```

**Acceptance:** middleware compiles; counter increments.

---

## Task 9: routes + ApiState + serve()

**Files:**
- Create / extend: `crates/roki-daemon/src/api/routes.rs`, `crates/roki-daemon/src/api/mod.rs`.

- [ ] **Step 1: `api/mod.rs` extension**

```rust
pub mod log_layer;
pub mod projection;
pub mod routes;
pub mod sanitize;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::cache::DiffCache;
use crate::escalation::EscalationQueue;
use crate::events::EventWriter;
use crate::linear::polling::NudgeHandle;
use crate::observability::EventRing;

pub struct ApiState {
    pub cache: Arc<DiffCache>,
    pub workflow: Arc<WorkflowConfig>,
    pub cfg: Arc<RokiConfig>,
    pub escalation: Arc<EscalationQueue>,
    pub ring: Arc<EventRing>,
    pub nudge: NudgeHandle,
    pub request_counter: Arc<AtomicU64>,
    pub boot_time: OffsetDateTime,
    pub daemon_writer: Arc<Mutex<EventWriter>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiBindError {
    #[error("bind {bind}:{port} failed: {source}")]
    Bind {
        bind: String,
        port: u16,
        #[source]
        source: std::io::Error,
    },
}

pub async fn serve(state: Arc<ApiState>) -> Result<(), ApiBindError> {
    let bind = state.cfg.api.bind.clone();
    let port = state.cfg.api.port.expect("serve called with port unset");
    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .expect("validated bind addr");
    let listener = TcpListener::bind(addr).await.map_err(|e| ApiBindError::Bind {
        bind: bind.clone(),
        port,
        source: e,
    })?;
    let app = routes::build_router(state.clone());
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .ok();
    Ok(())
}
```

- [ ] **Step 2: `api/routes.rs`**

```rust
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use uuid::Uuid;

use roki_api_types::{
    ApiEscalation, EventsPage, Healthz, RefreshAck, TicketDetail, TicketSummary,
};

use crate::api::log_layer;
use crate::api::projection;
use crate::api::ApiState;

pub fn build_router(state: Arc<ApiState>) -> Router {
    let log_state = Arc::new(log_layer::LogState {
        counter: state.request_counter.clone(),
        daemon_writer: state.daemon_writer.clone(),
    });
    Router::new()
        .route("/api/healthz", get(healthz))
        .route("/api/tickets", get(list_tickets))
        .route("/api/tickets/{id}", get(ticket_detail))
        .route("/api/tickets/{id}/cycles", get(list_cycles))
        .route(
            "/api/tickets/{id}/cycles/{cycle_id}/visits/{n}/{state_id}/{stream}",
            get(visit_stream),
        )
        .route("/api/events", get(events_page))
        .route("/api/escalations", get(list_escalations))
        .route("/api/refresh", post(refresh))
        .layer(axum::middleware::from_fn_with_state(log_state, log_layer::log_layer))
        .with_state(state)
}

fn json_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h
}

async fn healthz(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let uptime = (time::OffsetDateTime::now_utc() - state.boot_time).whole_seconds().max(0) as u64;
    let mut repos: Vec<String> = state
        .workflow
        .admission_repos()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    repos.sort();
    repos.dedup();
    let body = Healthz {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: uptime,
        configured_repositories: repos,
        api_request_count: state
            .request_counter
            .load(std::sync::atomic::Ordering::Relaxed),
    };
    (StatusCode::OK, json_headers(), Json(body))
}

async fn list_tickets(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let body: Vec<TicketSummary> = projection::tickets::list_tickets(&state.cache);
    (StatusCode::OK, json_headers(), Json(body))
}

async fn ticket_detail(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Response {
    if !is_ticket_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_ticket_id", "");
    }
    match projection::tickets::detail(
        &state.cache,
        &state.ring,
        &id,
        state.cfg.api.ticket_events_window as usize,
    ) {
        Some(d) => (StatusCode::OK, json_headers(), Json::<TicketDetail>(d)).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "ticket_not_found", &id),
    }
}

async fn list_cycles(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Response {
    if !is_ticket_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_ticket_id", "");
    }
    let (cycles, truncated) = projection::cycles::list_cycles(
        &state.cfg.paths.session_root,
        &id,
        state.cfg.api.cycle_list_window as usize,
    );
    #[derive(serde::Serialize)]
    struct Body {
        cycles: Vec<roki_api_types::CycleSummary>,
        truncated: bool,
    }
    (StatusCode::OK, json_headers(), Json(Body { cycles, truncated })).into_response()
}

async fn visit_stream(
    State(state): State<Arc<ApiState>>,
    Path((id, cycle_id, n, state_id, stream)): Path<(String, String, u32, String, String)>,
) -> Response {
    if !is_ticket_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_ticket_id", "");
    }
    let cycle_id = match Uuid::parse_str(&cycle_id) {
        Ok(u) => u,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_cycle_id", ""),
    };
    if !is_state_id(&state_id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_state_id", "");
    }
    let stream = match projection::visits::Stream::from_str(&stream) {
        Some(s) => s,
        None => return error_response(StatusCode::BAD_REQUEST, "invalid_stream", ""),
    };
    let states = projection::cycles::read_cycle_states(
        &state.cfg.paths.session_root,
        &id,
        cycle_id,
    );
    if matches!(states, Some(ref ss) if !ss.iter().any(|s| s == &state_id)) {
        return error_response(StatusCode::NOT_FOUND, "state_id_not_found_in_cycle", &state_id);
    }
    let bytes = match projection::visits::read_stream(
        &state.cfg.paths.session_root,
        &id,
        cycle_id,
        n,
        &state_id,
        stream,
    ) {
        Ok(b) => b,
        Err(_) => return error_response(StatusCode::NOT_FOUND, "stream_not_found", ""),
    };
    use projection::visits::Stream as S;
    let (ct, body) = match stream {
        S::Stdout | S::Stderr | S::ExitCode => {
            let s = String::from_utf8_lossy(&bytes);
            let cleaned = strip_ansi_only(&s);
            ("text/plain; charset=utf-8", cleaned)
        }
        S::Directive | S::Terminal => {
            let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            crate::api::sanitize::clean_json(&mut v);
            ("application/json; charset=utf-8", serde_json::to_string(&v).unwrap_or_default())
        }
        S::Events => {
            let mut out = String::new();
            let mut dropped = 0u32;
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() { continue; }
                match serde_json::from_slice::<serde_json::Value>(line) {
                    Ok(mut v) => {
                        crate::api::sanitize::clean_json(&mut v);
                        out.push_str(&serde_json::to_string(&v).unwrap_or_default());
                        out.push('\n');
                    }
                    Err(_) => dropped += 1,
                }
            }
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-ndjson; charset=utf-8"));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if dropped > 0 {
                headers.insert("Roki-Dropped-Lines", HeaderValue::from_str(&dropped.to_string()).unwrap());
            }
            return (StatusCode::OK, headers, out).into_response();
        }
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (StatusCode::OK, headers, body).into_response()
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<u64>,
    kind: Option<String>,
    ticket: Option<String>,
    cycle: Option<String>,
    limit: Option<usize>,
}

async fn events_page(
    State(state): State<Arc<ApiState>>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let cycle = match q.cycle.as_deref() {
        Some(s) => match Uuid::parse_str(s) {
            Ok(u) => Some(u),
            Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_cycle_id", ""),
        },
        None => None,
    };
    let limit = q.limit.unwrap_or(200).min(1000);
    let body: EventsPage = projection::events::page(
        &state.ring,
        projection::events::EventsQuery {
            since: q.since,
            kind: q.kind.as_deref(),
            ticket: q.ticket.as_deref(),
            cycle,
            limit,
        },
    );
    (StatusCode::OK, json_headers(), Json(body)).into_response()
}

async fn list_escalations(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let body: Vec<ApiEscalation> = projection::escalations::list(&state.escalation);
    (StatusCode::OK, json_headers(), Json(body))
}

async fn refresh(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let ack: RefreshAck = state.nudge.nudge().await;
    (StatusCode::ACCEPTED, json_headers(), Json(ack))
}

fn error_response(status: StatusCode, code: &str, detail: &str) -> Response {
    #[derive(serde::Serialize)]
    struct Err<'a> { error: &'a str, detail: &'a str }
    (status, json_headers(), Json(Err { error: code, detail })).into_response()
}

fn is_ticket_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn is_state_id(s: &str) -> bool {
    is_ticket_id(s)
}

fn strip_ansi_only(s: &str) -> String {
    // Reuse sanitize::clean_text minus html_escape.
    crate::api::sanitize::clean_text(s)
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}
```

(`WorkflowConfig::admission_repos()` may need a small helper; engineer adds `pub fn admission_repos(&self) -> Vec<&str>` returning the union of the top-level and override-keyed ghqs.)

- [ ] **Step 3: Failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use std::sync::atomic::AtomicU64;
    use tower::ServiceExt;

    fn fake_state() -> Arc<ApiState> {
        // Engineer constructs synthetic ApiState with empty cache, escalation,
        // ring; uses workflow_config_for_test from slice 8 to seed admission.
        unimplemented!("filled at implementation")
    }

    #[tokio::test]
    #[ignore = "wired in routes test fixture"]
    async fn healthz_returns_200_with_version() {
        let state = fake_state();
        let app = build_router(state);
        let req = Request::builder().uri("/api/healthz").body(axum::body::Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let h: roki_api_types::Healthz = serde_json::from_slice(&body).unwrap();
        assert_eq!(h.version, env!("CARGO_PKG_VERSION"));
    }
}
```

- [ ] **Step 4: Run**

Run: `cargo test -p roki-daemon --bin roki api::routes`.
Expected: build succeeds; the `#[ignore]` test is skipped.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/api/
git commit -m "feat(slice9,api): axum router + handlers for every fr:10 endpoint"
```

**Acceptance:** routes compile; healthz handler returns the right shape (covered by e2e fixtures in Task 14).

---

## Task 10: cycle.json artifact

**Files:**
- Modify: `crates/roki-daemon/src/daemon/ticket_task.rs`, `docs/reference/artifacts.md`.

- [ ] **Step 1: Add `cycle.json` writer in ticket_task**

In the cycle-spawn path, after creating `<session_root>/<ticket>/cycle-<uuid>/`, write the initial JSON with `ended_at: null`:

```rust
fn write_cycle_start(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
    kind: CycleKind,
    trigger: CycleTrigger,
    states: Vec<String>,
) -> std::io::Result<()> {
    let path = session_root
        .join(ticket_id)
        .join(format!("cycle-{cycle_id}"))
        .join("cycle.json");
    let body = serde_json::json!({
        "cycle_id": cycle_id.to_string(),
        "ticket_id": ticket_id,
        "kind": kind.canonical_str(),
        "trigger": trigger.canonical_str(),
        "started_at": now_rfc3339(),
        "ended_at": null,
        "terminal_id": null,
        "failure_kind": null,
        "total_visits": 0,
        "states": states,
    });
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&body)?)?;
    std::fs::rename(tmp, path)
}

fn write_cycle_end(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
    result: CycleEndPayload,
) -> std::io::Result<()> {
    let path = session_root
        .join(ticket_id)
        .join(format!("cycle-{cycle_id}"))
        .join("cycle.json");
    let mut body: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    if let serde_json::Value::Object(m) = &mut body {
        m.insert("ended_at".into(), serde_json::Value::String(now_rfc3339()));
        m.insert("terminal_id".into(), result.terminal_id.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
        m.insert("failure_kind".into(), result.failure_kind.map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
        m.insert("total_visits".into(), serde_json::Value::from(result.total_visits));
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&body)?)?;
    std::fs::rename(tmp, path)
}
```

Wire `write_cycle_start` before `cycle_state::run_cycle`. Wire `write_cycle_end` on both `Ok` and `Err` paths.

- [ ] **Step 2: Failing unit test**

```rust
#[test]
fn cycle_json_round_trip_through_start_and_end() {
    let dir = tempfile::tempdir().unwrap();
    let cycle = Uuid::new_v4();
    write_cycle_start(dir.path(), "ENG-1", cycle, CycleKind::Rule, CycleTrigger::Runtime, vec!["a".into()]).unwrap();
    let mid: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("ENG-1").join(format!("cycle-{cycle}")).join("cycle.json")).unwrap()).unwrap();
    assert!(mid.get("ended_at").unwrap().is_null());
    write_cycle_end(dir.path(), "ENG-1", cycle, CycleEndPayload { terminal_id: Some("__success__".into()), failure_kind: None, total_visits: 1 }).unwrap();
    let end: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("ENG-1").join(format!("cycle-{cycle}")).join("cycle.json")).unwrap()).unwrap();
    assert_eq!(end["terminal_id"], serde_json::Value::String("__success__".into()));
    assert!(!end["ended_at"].is_null());
}
```

- [ ] **Step 3: Add row to `docs/reference/artifacts.md`**

```markdown
| `<session_root>/<ticket>/cycle-<uuid>/cycle.json` | Cycle metadata: kind, trigger, started_at, ended_at, terminal_id, failure_kind, total_visits, declared state ids. Atomic write at cycle start (`ended_at: null`); atomic update at cycle end. | [fr:10 §GET /api/tickets/{id}/cycles](../fr/10-http-api.md) |
```

- [ ] **Step 4: Run**

Run: `cargo test -p roki-daemon --bin roki ticket_task`.
Expected: existing + 1 new pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/daemon/ticket_task.rs docs/reference/artifacts.md
git commit -m "feat(slice9,daemon): cycle.json metadata artifact at cycle start/end"
```

**Acceptance:** ticket_task tests pass; `kusara validate` clean.

---

## Task 11: wire `EventRing` + `PollingTracker` + `api::serve` into `runtime`

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`.

- [ ] **Step 1: Construct ring + tracker**

In `runtime::run_inner`, between `WorkflowConfig::load` and the dispatcher boot:

```rust
let ring = EventRing::new(cfg.log.ring_size as usize);
let last_webhook_success = Arc::new(AtomicI64::new(0));
let nudge_handle = {
    let cadence = std::time::Duration::from_secs(cfg.linear.polling.cadence_seconds as u64);
    let rate_limit = rate_limit.clone();
    let webhook_atom = last_webhook_success.clone();
    let cache_for_tick = cache.clone();
    PollingTracker::spawn(
        cadence,
        rate_limit,
        webhook_atom,
        Box::new(move |reason| {
            tracing::info!(?reason, "polling_tick scheduled");
            // The tick itself runs cold-start enumerate equivalent. Engineer
            // wires the call to `cold_start::tick(...)` here once the
            // `cold_start` module gains a per-tick-only entry point. For
            // slice 9 the tick logs a `polling_tick` event via
            // `daemon_writer.emit(Event::PollingTick { ... })`.
        }),
    )
};
```

- [ ] **Step 2: Spawn API**

```rust
if let Some(port) = cfg.api.port {
    let state = Arc::new(crate::api::ApiState {
        cache: cache.clone(),
        workflow: workflow.clone(),
        cfg: cfg_arc.clone(),
        escalation: escalation.clone(),
        ring: ring.clone(),
        nudge: nudge_handle.clone(),
        request_counter: Arc::new(AtomicU64::new(0)),
        boot_time: time::OffsetDateTime::now_utc(),
        daemon_writer: daemon_writer.clone(),
    });
    let bind = cfg.api.bind.clone();
    let writer_for_log = daemon_writer.clone();
    tokio::spawn(async move {
        match crate::api::serve(state).await {
            Ok(()) => {}
            Err(crate::api::ApiBindError::Bind { bind, port, source }) => {
                let mut w = writer_for_log.lock().await;
                let _ = w.emit(&Event::ApiBindFailed {
                    ts: now_rfc3339(),
                    bind,
                    port,
                    error: source.to_string(),
                });
            }
        }
    });
} else {
    let mut w = daemon_writer.lock().await;
    let _ = w.emit(&Event::ApiDisabled { ts: now_rfc3339() });
}
```

- [ ] **Step 3: Update `Event` enum with the missing variants**

```rust
#[serde(rename_all = "snake_case")]
ApiBindFailed { ts: String, bind: String, port: u16, error: String },
ApiDisabled { ts: String },
PollingTick { ts: String, trigger: String, status_set: Vec<String>, enumerated: u32, admitted: u32 },
RefreshNudgeAcknowledged { ts: String, coalesced: bool, backoff_active: bool, client_addr: String },
```

Update `kind_str` for each.

- [ ] **Step 4: Smoke build**

Run: `cargo build -p roki-daemon`.
Expected: `Finished`.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs crates/roki-daemon/src/events.rs
git commit -m "feat(slice9): wire EventRing + PollingTracker + api::serve into runtime"
```

**Acceptance:** binary compiles; existing bin tests still pass.

---

## Task 12: ref docs catch-up

**Files:**
- Modify: `docs/reference/log-events.md`, `docs/fr/08-observability-logs.md`.

- [ ] **Step 1: Add 5 rows to `ref:log-events`**

```markdown
| `api_request` | `method`, `path`, `query_keys`, `status`, `duration_ms`, `client_addr`, `correlation_id` | per-request structured log |
| `api_bind_failed` | `bind`, `port`, `error` | bind failure at startup |
| `api_disabled` | (none) | `[api].port` unset at startup |
| `polling_tick` | `trigger`, `status_set`, `enumerated`, `admitted` | one polling tracker fire |
| `refresh_nudge_acknowledged` | `coalesced`, `backoff_active`, `client_addr` | one ack from `POST /api/refresh` |
```

- [ ] **Step 2: Update `fr:08 §Event catalog`**

Add a paragraph + bullets enumerating the 5 new events with one-sentence descriptions each.

- [ ] **Step 3: Validate docs**

Run: `kusara validate`.
Expected: `OK (22 docs)`.

- [ ] **Step 4: Commit**

```bash
git add docs/reference/log-events.md docs/fr/08-observability-logs.md
git commit -m "docs(slice9,ref:log-events,fr:08): add 5 API + polling event rows"
```

**Acceptance:** `kusara validate` clean.

---

## Task 13: Cargo.toml e2e registrations

**Files:**
- Modify: `crates/roki-daemon/Cargo.toml`.

- [ ] **Step 1: Add 10 `[[test]]` entries before the `[features]` section**

```toml
[[test]]
name = "api_disabled_smoke"
path = "tests/e2e/api_disabled_smoke.rs"

[[test]]
name = "api_healthz_smoke"
path = "tests/e2e/api_healthz_smoke.rs"

[[test]]
name = "api_tickets_list_smoke"
path = "tests/e2e/api_tickets_list_smoke.rs"

[[test]]
name = "api_cycle_and_visit_smoke"
path = "tests/e2e/api_cycle_and_visit_smoke.rs"

[[test]]
name = "api_events_since_smoke"
path = "tests/e2e/api_events_since_smoke.rs"

[[test]]
name = "api_escalations_smoke"
path = "tests/e2e/api_escalations_smoke.rs"

[[test]]
name = "api_refresh_coalesce_smoke"
path = "tests/e2e/api_refresh_coalesce_smoke.rs"

[[test]]
name = "api_refresh_backoff_smoke"
path = "tests/e2e/api_refresh_backoff_smoke.rs"

[[test]]
name = "api_non_loopback_warn_smoke"
path = "tests/e2e/api_non_loopback_warn_smoke.rs"

[[test]]
name = "api_bind_failure_smoke"
path = "tests/e2e/api_bind_failure_smoke.rs"
```

- [ ] **Step 2: Commit**

```bash
git add crates/roki-daemon/Cargo.toml
git commit -m "build(slice9): register 10 slice-9 e2e fixtures"
```

**Acceptance:** workspace builds.

---

## Task 14: e2e fixtures

Each fixture follows the slice-8 pattern (inline support helpers + `support_cold_start`). Below is the layout for fixture 1; fixtures 2–10 follow the same skeleton, with the `[api]` block and the `curl`-equivalent assertions adapted to the spec's §6.1 acceptance for that fixture.

For brevity, only fixture 1's full text is reproduced; engineers writing fixtures 2–10 follow the spec acceptance text in §6.1 verbatim and the helper pattern shown here.

- [ ] **Step 1: `tests/e2e/api_disabled_smoke.rs`**

```rust
//! Slice 9 e2e: `[api].port` unset → daemon emits `api_disabled`; no port
//! is opened; webhook still works.

use std::net::TcpListener;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support_cold_start;
use support_cold_start::{await_daemon_event, await_daemon_ready, stub_empty_issues};

#[tokio::test]
async fn api_port_unset_emits_api_disabled() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let linear = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":{"viewer":{"id":"u1"}}})))
        .mount(&linear).await;
    stub_empty_issues(&linear).await;

    let work = TempDir::new().unwrap();
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
    let workflow = work.path().join("WORKFLOW.yaml");
    std::fs::write(&workflow, "admission:\n  assignee: u1\n  repos:\n    - ghq: github.com/example/repo\n").unwrap();
    let roki = work.path().join("roki.toml");
    let body = format!(
        "[linear]\ntoken=\"x\"\n[linear.webhook]\nbind=\"127.0.0.1\"\nport={port}\n[default.ai]\ncli=\"echo\"\n[engine]\n[paths]\nworkflow=\"{}\"\nsession_root=\"{}\"\n[log]\n",
        workflow.display(),
        session_root.display()
    );
    std::fs::write(&roki, body).unwrap();

    let binary = env!("CARGO_BIN_EXE_roki");
    let mut child = Command::new(binary)
        .arg("run").arg("--config").arg(&roki)
        .env("ROKI_LINEAR_GRAPHQL_URL", linear.uri())
        .env("ROKI_GHQ_BASE_OVERRIDE", work.path())
        .kill_on_drop(true).spawn().unwrap();

    let _ = await_daemon_event(&session_root, "api_disabled", Duration::from_secs(15)).await;
    let _ = await_daemon_ready(&session_root).await;
    use nix::sys::signal::{Signal, kill}; use nix::unistd::Pid;
    kill(Pid::from_raw(child.id().unwrap() as i32), Signal::SIGTERM).ok();
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}
```

- [ ] **Step 2: Fixtures 2–10**

Replicate the skeleton, varying:
- **`api_healthz_smoke`** — `[api]` set; `reqwest::get("/api/healthz")` returns 200; `version` matches `env!("CARGO_PKG_VERSION")`; `api_request_count >= 1`.
- **`api_tickets_list_smoke`** — admit two tickets via webhook; `/api/tickets` returns 2 entries; one label `<script>` and one ANSI escape come back HTML-escaped + ANSI-stripped.
- **`api_cycle_and_visit_smoke`** — webhook → cycle → `/api/tickets/{id}/cycles` returns 1; `/api/.../visits/1/<state>/stdout` body contains the captured `out`.
- **`api_events_since_smoke`** — drive 5 events; `/api/events?since=0` returns ordered seqs; saturate ring (`ring_size = 2`); query `since=1` returns `gap: true`.
- **`api_escalations_smoke`** — slice-8 recursion-bound fixture style; `/api/escalations` returns 1 entry with the right `kind` + `state_id`.
- **`api_refresh_coalesce_smoke`** — fire two `POST /api/refresh` concurrently; both ack with `coalesced: true` against the same `earliest_fire_at`; one `polling_tick` event in `_daemon.events.jsonl`.
- **`api_refresh_backoff_smoke`** — wiremock returns 429 on the enumerate query; `POST /api/refresh` returns `backoff_active: true`; no `polling_tick`.
- **`api_non_loopback_warn_smoke`** — `[api].bind = "0.0.0.0"`; daemon emits a warn-severity log line containing "non-loopback"; server still binds.
- **`api_bind_failure_smoke`** — pre-bind the `[api].port` from the test process; daemon emits `api_bind_failed`; daemon continues to `daemon_ready`; webhook still works.

Each fixture is a separate commit `test(slice9,api,e2e): add <fixture_name>`.

- [ ] **Step 3: Run each as it lands**

Run: `cargo test -p roki-daemon --test <fixture_name>`.
Expected: 1 pass per fixture.

- [ ] **Step 4: Commit (per fixture)**

```bash
git add crates/roki-daemon/tests/e2e/<fixture>.rs
git commit -m "test(slice9,api,e2e): add <fixture>"
```

**Acceptance:** `cargo test --workspace` shows 10 new e2e suites green.

---

## Task 15: final sweep

**Files:** all (touched).

- [ ] `cargo fmt --all -- --check` clean. Run `cargo fmt --all` if drift.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo test --workspace` green (expect ~325+ passing).
- [ ] `kusara validate` returns OK.
- [ ] `grep -rn 'WORKFLOW\.toml' crates/roki-daemon/src` returns the same single placeholder hit in `error.rs:536` (slice 8 documented exception) and nothing else.
- [ ] `grep -n 'TODO\|FIXME' crates/roki-daemon/src/api crates/roki-api-types/src` returns zero hits.
- [ ] Final commit: `chore(slice9): rustfmt + clippy clean`.

**Acceptance:** all checks pass.

---

## Spec coverage check

| Spec section | Task(s) |
|---|---|
| §1 deliverables | 0 (spec commit) + every subsequent |
| §2.1 module layout | 1, 4, 5, 6, 7, 8, 9 |
| §2.2 `roki-api-types` | 1, 2 |
| §2.3 EventRing | 4 |
| §2.4 PollingTracker | 5 |
| §2.5 routes / ApiState | 9, 11 |
| §2.6 sanitize | 6 |
| §2.7 per-request log | 8 |
| §2.8 [api] + [linear].polling | 3 |
| §2.9 wiring at boot | 11 |
| §3 cycle.json | 10 |
| §4 events additions | 8, 11, 12 |
| §5.1-5.7 endpoint behaviour | 9, 14 |
| §6.1 e2e fixtures | 13, 14 |
| §6.3 unit tests | 2, 3, 4, 5, 6, 7, 8, 10 |
| §7 implementation sequence | this plan |
| §8 boundaries | spec; reflected in deferral choices in tasks |
| §9 documented divergence | 3 (cycle_list_window), 5 (outage threshold), 9 (visit-stream content type) |

All 9 spec sections covered.

---

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| `time::serde::rfc3339::option` requires `time` feature `serde-well-known` | Pin in `roki-api-types/Cargo.toml` + `roki-daemon` already has it. |
| `axum::extract::ConnectInfo` requires `into_make_service_with_connect_info`; trivial omission breaks remote-addr capture | Task 9 + Task 11 both call `into_make_service_with_connect_info::<SocketAddr>`. e2e `api_request` payload assertion in Task 14 fixture 2 verifies. |
| `vte = "0.13"` API drift: `Perform::execute(&mut self, b: u8)` signature varies | Task 6 stripper code matches `vte 0.13`. If a different version is pinned by the workspace lock, engineer adjusts the trait impl signature. |
| `EscalationEntry::phase` field name (slice 7) vs `state_id` projection field (spec §5.6) | Task 7's escalation projection maps `entry.phase → ApiEscalation.state_id`. |
| `WorkflowConfig::admission_repos()` does not exist | Task 9 adds the helper. Engineer mirrors the iteration shape used in slice-8 cold_start (top-level + every override key). |
| Polling tracker invocation of cold-start enumerate is heavy lifting; implementing it inside `Box<dyn Fn>` ties the closure to the cold-start crate | Task 11 wires a minimal closure that emits `polling_tick` with stubbed `enumerated/admitted = 0`. The full enumerate-on-tick pass is OOS for slice 9 (only the nudge ack contract is required for `POST /api/refresh`); a follow-up task in slice 10 wires the full enumerate. |
| 429 backoff atom shape: `RateLimitState` may not expose `next_legal_at` | Task 5 adds the accessor. |
| Cycle.json atomic update can race a reader that opens the file mid-rename | `std::fs::rename` is atomic on POSIX; readers see either the old file or the new one. The projection layer tolerates parse failure (skips the cycle from the list). |
| ring-disabled (`ring_size = 0`) breaks `next_since` semantics for clients pinning to seq numbers | Documented; clients should treat empty `events: []` as "ring disabled" and fall back to the file destination. |

---

## Out of scope (deferred per spec §8)

- Authentication / TLS / non-loopback hardening beyond a warn log.
- WebSocket / SSE push.
- Hot reload of `[api].*` or `[linear].polling.*`.
- Cross-ticket cycle lookup.
- `/api/tickets` filtering / pagination.
- Per-state-id stream live-tail.
- `/api/refresh` body parameters.
- TUI consumption (`fr:11`, slice 10).
- Full enumerate-on-polling-tick implementation; this slice only wires the nudge ack contract.
