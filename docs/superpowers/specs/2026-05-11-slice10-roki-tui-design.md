# Slice 10 — roki-tui Design

Date: 2026-05-11
Scope: Implement `fr:11` end to end — a standalone `ratatui` binary that polls the slice-9 HTTP API and renders the four documented views (tickets, ticket detail, events, escalations) with local-only escalation acknowledgement and a manual refresh action. Introduce the `roki-tui` workspace member, a `~/.config/roki-tui/config.toml` loader, and a small additive extension to `roki-api-types::CycleSummary` so the ticket-detail view can resolve the cycle's last-spawned state without touching disk.

## 1. Position in the Roadmap

Slice 10 closes:

- `roki-tui-foundation` — full `fr:11` surface: `roki-tui <api-url>` binary, four views, `[polling]` cadences (TOML + CLI override), defense-in-depth ANSI / control-char strip, terminal palette detection with 24-bit / 256-color / 16-color fallback, structured startup log to its own stderr, Windows-not-supported exit.
- `roki-tui-refresh-and-ack` — `POST /api/refresh` action with debouncing matching the daemon-side coalescing cap, and local-only escalation acknowledgement keyed on `(marker, ticket_id, cycle_id, kind, state_id, visit_n)`.
- `roki-api-types-extension` — additive `CycleSummary::last_state_id: Option<String>` field plus a projection mapping in `roki-daemon::api::projection::cycles` so the TUI ticket-detail tail can pick the right `{state_id}` for `GET /api/tickets/{id}/cycles/{cycle_id}/visits/{n}/{state_id}/stdout` without consulting `cycle.json` directly.

Slices 1–9 provide: cycle engine, escalation queue, persistent dispatcher with diff cache, structured event writer + ring, observability HTTP API on loopback, polling tracker + refresh nudge, `roki-api-types` schema crate, `cycle.json` per-cycle metadata file.

Out of scope, deferred:

- **`roki events --tail` / `roki log --follow`** (`fr:09`). Slice 11 wires the CLI live-tail against the same `/api/events?since=<seq>` model the TUI uses here.
- **Daemon-side escalation ack persistence**. `fr:11 §Escalation acknowledgement` is local UI only in v1.
- **Persistent UI state** (selection memory between sessions). `fr:11 §Boundaries`.
- **State-transition diagram view**. `fr:11 §Boundaries` (every ticket is `idle` or `cycling`; the cycle id is the live indicator).
- **Mutating actions beyond `POST /api/refresh`** (cancel / retry / pause). `fr:11 §Boundaries`.
- **Web UI**. `fr:11 §Boundaries`.
- **Authentication / TLS**. v1 loopback assumption (`fr:11 §Boundaries`).
- **Windows support**. Compiled out behind `#[cfg(not(windows))]`; on Windows `main` prints `roki-tui: Windows is not supported in v1` to stderr and exits 1 (`fr:11 §Terminal compatibility`).

---

## 2. Architecture

### 2.1 Workspace + crate layout

```
crates/
└── roki-tui/                       // NEW workspace member, binary crate
    ├── Cargo.toml
    └── src/
        ├── main.rs                 // tokio main, terminal setup/restore, top-level loop
        ├── cli.rs                  // clap parser (positional api-url + cadence overrides)
        ├── config.rs               // ~/.config/roki-tui/config.toml loader + validation
        ├── client/
        │   ├── mod.rs              // ApiClient handle (reqwest)
        │   ├── tickets.rs          // fetch_tickets, fetch_ticket_detail
        │   ├── cycles.rs           // fetch_cycles, fetch_visit_stdout
        │   ├── events.rs           // fetch_events_since
        │   ├── escalations.rs      // fetch_escalations
        │   └── refresh.rs          // post_refresh
        ├── model/
        │   ├── mod.rs              // AppModel (tickets, events, escalations, focus, status)
        │   ├── tickets.rs          // TicketsView state, sort key
        │   ├── ticket_detail.rs    // selected ticket + cycles + tail buffer
        │   ├── events.rs           // EventsView state (filters, last_seq, gap flag)
        │   └── escalations.rs      // EscalationsView state + ack set
        ├── poll/
        │   ├── mod.rs              // PollScheduler — three independent cadences, mpsc Updates
        │   ├── tickets.rs
        │   ├── events.rs
        │   └── escalations.rs
        ├── ui/
        │   ├── mod.rs              // App::draw entrypoint, view dispatch
        │   ├── tickets.rs          // ratatui Table widget for tickets view
        │   ├── ticket_detail.rs    // split layout: cycles table + stdout tail Paragraph
        │   ├── events.rs           // event stream List
        │   ├── escalations.rs      // escalations Table with ack glyph
        │   └── status_bar.rs       // bottom status line
        ├── input.rs                // crossterm key dispatch (view switch, quit, ack, refresh)
        ├── sanitize.rs             // ansi_strip + control_strip (defense in depth)
        ├── palette.rs              // 24-bit / 256-color / 16-color detection
        └── startup_log.rs          // structured JSON line to stderr at startup
```

### 2.2 Cargo manifest

```toml
[package]
name = "roki-tui"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "Terminal UI for the roki daemon's observability HTTP API"

[[bin]]
name = "roki-tui"
path = "src/main.rs"

[dependencies]
roki-api-types = { path = "../roki-api-types" }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "sync", "time", "signal"] }
reqwest = { workspace = true, features = ["json"] }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
toml = "0.8"
time = { workspace = true, features = ["formatting", "serde-well-known"] }
uuid = { workspace = true, features = ["serde"] }
clap = { workspace = true, features = ["derive"] }
ratatui = { version = "0.28", default-features = false, features = ["crossterm"] }
crossterm = "0.28"
thiserror = { workspace = true }
tracing = { workspace = true }
unicode-width = "0.1"
anstyle-parse = "0.2"          # ANSI escape stripping
directories = "5"              # XDG-honoring ~/.config/roki-tui resolution

[lints]
workspace = true
```

Workspace root `Cargo.toml`: append `"crates/roki-tui"` to `[workspace.members]` (the comment block in the manifest already anticipates this).

### 2.3 Async runtime topology

`tokio::main(flavor = "multi_thread", worker_threads = 2)`. Three long-lived tasks plus the render loop:

- **`PollScheduler::tickets`** — fires every `cfg.polling.tickets_seconds`. Calls `client::fetch_tickets`, sends `Update::Tickets(Vec<TicketSummary>)` over an `mpsc::Sender<Update>` (capacity 32).
- **`PollScheduler::events`** — fires every `cfg.polling.events_seconds`. Tracks `last_seq` per session. Calls `client::fetch_events_since(last_seq)`; sends `Update::Events { page, requested_since }`. On `gap: true` the UI surfaces a status bar warning until the next successful tail catches up.
- **`PollScheduler::escalations`** — fires every `cfg.polling.escalations_seconds`. Calls `client::fetch_escalations`; sends `Update::Escalations(Vec<ApiEscalation>)`. The ack set is reconciled against the new snapshot inside `AppModel::apply_escalations` (entries no longer present have their ack cleared).
- **`PollScheduler::ticket_detail`** — only active when the ticket-detail view is focused. Fires every `cfg.polling.tickets_seconds`. Calls `fetch_ticket_detail`, `fetch_cycles(ticket_id)`, and `fetch_visit_stdout` for the currently selected cycle's last visit.

The render loop owns `AppModel`, drains the `Update` channel without blocking, then `terminal.draw`s the active view. A separate task forwards `crossterm::event::EventStream` into the same channel as `Update::Input(KeyEvent)` so the render loop has a single ordering point.

### 2.4 Failure-mode budget

- **Connection refused / DNS / non-2xx**: each poll task emits a one-line status-bar message `<view>: HTTP <status> (<short error>)`. The TUI never exits on poll failure (`fr:11 §Startup and connection`). Successful next poll clears the status bar.
- **Schema drift** (`serde_json::from_slice` fails): the client logs the failure to stderr (single line, no body) and surfaces `events: schema error` / etc. in the status bar. The corresponding view keeps the last successful snapshot until a parsed response arrives.
- **Terminal resize**: handled implicitly by `ratatui`'s `Terminal::draw` re-layout. No state changes.
- **SIGINT / SIGTERM / quit key**: restore the terminal (`disable_raw_mode`, `LeaveAlternateScreen`, `DisableMouseCapture`), then exit 0.

---

## 3. `roki-api-types` extension

Add one optional field to `CycleSummary`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleSummary {
    // ... existing fields ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_state_id: Option<String>,
}
```

Projection (`roki-daemon::api::projection::cycles`): map `cycle.json::states.last()` into `last_state_id`. For an in-flight cycle (`ended_at: null`) the field tracks the most recently appended state id; for a completed cycle the field equals `terminal_id`. Forward-compat fallback: if `cycle.json` is from a slice-9 daemon that does not yet write `states[]` (it does — slice 9 wrote it), the field becomes `None` and the TUI suppresses the tail Paragraph.

This addition is covered by `fr:10 §Additive-by-default`. Round-trip test added to `roki-api-types/src/tickets.rs`. No client compatibility break: the existing slice-9 e2e fixtures continue to assert on the fields they assert on today.

---

## 4. Configuration

### 4.1 `~/.config/roki-tui/config.toml`

Resolved via the `directories` crate (`ProjectDirs::from("", "", "roki-tui")`) which respects `XDG_CONFIG_HOME` on Linux and falls back to `~/.config/roki-tui/config.toml` on macOS for parity with the documented path in `fr:11 §Configuration`. (macOS users with no `XDG_CONFIG_HOME` get `~/Library/Application Support/roki-tui/config.toml` only if they opt in via `XDG_CONFIG_HOME`; the documented `~/.config/roki-tui/config.toml` is the canonical path the loader checks first.)

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiConfig {
    #[serde(default)]
    pub polling: PollingSection,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollingSection {
    #[serde(default = "PollingSection::default_tickets")]
    pub tickets_seconds: u32,           // default 2; min 1
    #[serde(default = "PollingSection::default_events")]
    pub events_seconds: u32,            // default 1; min 1
    #[serde(default = "PollingSection::default_escalations")]
    pub escalations_seconds: u32,       // default 5; min 1
}
```

Missing file → defaults. Validation failure refuses startup with a one-line error on stderr containing the offending key + the rejected value (matches `fr:11 §Configuration`).

### 4.2 CLI

```
roki-tui <API_URL> [--config <path>]
                   [--tickets-cadence <secs>]
                   [--events-cadence <secs>]
                   [--escalations-cadence <secs>]
```

- `<API_URL>`: required, positional. Parsed via `reqwest::Url::parse`. Failure → stderr + exit 2.
- `--config <path>`: override the default config file location.
- `--*-cadence`: override the matching `[polling]` field. Validation rules identical to the TOML loader. Override precedence: CLI > config file > default.

`ref:cli` gains a new subcommand section `roki-tui` (rendered as a sibling of `roki run` / `roki events` etc.) listing these four flags. The subcommand table at the top of `ref:cli` gains a row mapping `roki-tui` → `fr:11`.

### 4.3 Validation

| Key | Constraint | Error code |
|---|---|---|
| `polling.tickets_seconds` | `>= 1` | `invalid_tickets_cadence` |
| `polling.events_seconds` | `>= 1` | `invalid_events_cadence` |
| `polling.escalations_seconds` | `>= 1` | `invalid_escalations_cadence` |
| `api_url` (positional) | parseable by `reqwest::Url::parse`; scheme ∈ `http`/`https`; non-empty host | `invalid_api_url` |
| `--config <path>` | exists and readable when supplied explicitly | `config_not_found` |

Default config file absent → silently fall back to defaults (`fr:11 §Configuration` says the file is optional). Explicit `--config <path>` missing → hard error.

---

## 5. HTTP client

Single `ApiClient` wrapping `reqwest::Client` (one shared connection pool, `tcp_keepalive = 60s`, `pool_idle_timeout = 120s`, `timeout = 5s` per request). Base URL is the parsed `api_url`. Every request sets `Accept: application/json`.

```rust
pub struct ApiClient {
    base: reqwest::Url,
    http: reqwest::Client,
}

impl ApiClient {
    pub async fn fetch_tickets(&self) -> Result<Vec<TicketSummary>, ClientError>;
    pub async fn fetch_ticket_detail(&self, id: &str) -> Result<TicketDetail, ClientError>;
    pub async fn fetch_cycles(&self, id: &str) -> Result<Vec<CycleSummary>, ClientError>;
    pub async fn fetch_events_since(&self, since: Option<u64>) -> Result<EventsPage, ClientError>;
    pub async fn fetch_escalations(&self) -> Result<Vec<ApiEscalation>, ClientError>;
    pub async fn fetch_visit_stdout(
        &self,
        id: &str,
        cycle: Uuid,
        visit_n: u32,
        state_id: &str,
    ) -> Result<String, ClientError>; // text/plain body
    pub async fn post_refresh(&self) -> Result<RefreshAck, ClientError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),
    #[error("invalid utf-8 in text response")]
    InvalidUtf8,
}
```

Every JSON response goes through `clean_json` (`§7`) before being handed to the model layer, even though the server already sanitizes. Stdout / text responses go through `ansi_strip` + `control_strip`.

---

## 6. View model + rendering

### 6.1 `AppModel`

```rust
pub struct AppModel {
    pub focus: View,                       // Tickets / TicketDetail / Events / Escalations
    pub tickets: TicketsView,
    pub ticket_detail: TicketDetailView,   // populated lazily on focus
    pub events: EventsView,
    pub escalations: EscalationsView,
    pub status: StatusLine,
    pub refresh: RefreshState,             // Idle / InFlight / DebouncedAt(Instant)
    pub palette: Palette,                  // RGB24 / IndexedAnsi256 / IndexedAnsi16
}

pub struct TicketsView {
    pub rows: Vec<TicketSummary>,          // sorted by last_event_at desc
    pub selected: usize,
    pub last_refresh_at: Option<Instant>,
}

pub struct TicketDetailView {
    pub ticket_id: Option<String>,
    pub detail: Option<TicketDetail>,
    pub cycles: Vec<CycleSummary>,
    pub selected_cycle: usize,
    pub tail_text: Option<String>,         // ANSI-stripped stdout tail
    pub tail_visit_n: Option<u32>,
}

pub struct EventsView {
    pub rows: VecDeque<ApiEvent>,          // capped at 1000 in-memory
    pub last_seq: Option<u64>,
    pub gap_pending: bool,
    pub filter: EventsFilter,              // (kind?, ticket?, cycle?) — driven by '/' input later
}

pub struct EscalationsView {
    pub rows: Vec<ApiEscalation>,
    pub acked: HashSet<AckKey>,
    pub selected: usize,
}

pub struct AckKey {
    pub marker: String,
    pub ticket_id: Option<String>,
    pub cycle_id: Option<Uuid>,
    pub kind: String,
    pub state_id: Option<String>,
    pub visit_n: Option<u32>,
}
```

`apply_escalations` rebuilds the ack set: for every existing ack key, retain it only if the new snapshot still contains a matching entry. This is what `fr:11 §Escalation acknowledgement` calls "auto clear when the corresponding escalation no longer appears in the next API snapshot".

### 6.2 Rendering (ratatui)

Each view is a stateless `widgets::*::StatefulWidget` constructed from `AppModel` on each `terminal.draw` tick:

- **Tickets** — `Table` with columns: ticket id / repo / status / labels / assignee / in-flight cycle id / last event. Highlighted row is `palette.highlight()`.
- **Ticket detail** — `Layout::vertical([Constraint::Length(8), Constraint::Min(0)])`. Top: cycles `Table` (id / kind / trigger / started / ended / terminal / visits). Bottom: `Paragraph` with `tail_text` (`Wrap { trim: false }`, scroll tracked in `TicketDetailView`).
- **Events** — `List` rendered from `EventsView::rows`. Each item: `ts kind ticket cycle payload-preview`. Payload preview truncated to terminal width.
- **Escalations** — `Table` with columns: kind / state id / ticket / cycle / error text. Acked rows render with both a `[*]` glyph prefix and a `palette.acked()` color modifier (`fr:11 §Escalation acknowledgement` requires the non-color glyph for Terminal.app parity).

Common chrome:

- One-line status bar at the bottom: `View=<name> | tickets <n>s, events <n>s, escalations <n>s | last error: <…>`.
- Tab strip at top: `[1]Tickets [2]TicketDetail [3]Events [4]Escalations`. Highlights the focused view.

### 6.3 Key bindings

| Key | Action |
|---|---|
| `q`, `Ctrl-C` | Quit (restore terminal, exit 0). |
| `1` / `2` / `3` / `4` | Switch focus (Tickets / TicketDetail / Events / Escalations). |
| `Up` / `Down`, `k` / `j` | Move row selection in the focused view. |
| `Enter` (in Tickets) | Open ticket detail for the selected ticket. |
| `Enter` (in TicketDetail) | Reload the selected cycle's tail. |
| `r` | Manual refresh: fires `POST /api/refresh`, updates the status bar with the ack outcome. |
| `a` (in Escalations) | Toggle local ack on the selected escalation row. |
| `c` (in TicketDetail) | Copy-print the matching `roki log` command line into the status bar so the operator can copy it (`fr:11 §Log inspection`). |

All other keys are no-ops in v1.

---

## 7. Sanitization (defense in depth)

```rust
// sanitize.rs

/// Strip ANSI escape sequences using the `anstyle-parse` state machine.
pub fn ansi_strip(s: &str) -> String;

/// Remove C0 control characters except `\n` and `\t`, plus DEL (0x7F).
pub fn control_strip(s: &str) -> String;

/// Apply `ansi_strip` then `control_strip` to every string leaf in a JSON value.
pub fn clean_json(value: &mut serde_json::Value);
```

Applied unconditionally to every JSON response (`clean_json`) and every text-stream response (`ansi_strip` then `control_strip`). Server-side sanitization (`fr:10 §Sanitization`) is the first line of defense; this layer is the documented second line (`fr:11 §Defense-in-depth sanitization`). Unit tests in `sanitize.rs::tests` cover: bare CSI escape, OSC escape, `\x07` BEL, `\x1b]` open-ended OSC, `\r`, `\x00`, multi-byte UTF-8 boundaries (the strip must not split a code point).

---

## 8. Terminal palette detection

```rust
pub enum Palette {
    Rgb24,
    IndexedAnsi256,
    IndexedAnsi16,
}

pub fn detect() -> Palette;
```

Detection order, first match wins:

1. `$COLORTERM` ∈ {`truecolor`, `24bit`} → `Rgb24`.
2. `$TERM` matches `*-256color` (incl. `tmux-256color`, `xterm-256color`, `screen-256color`) → `IndexedAnsi256`.
3. Otherwise → `IndexedAnsi16`.

When the resolved palette is `IndexedAnsi16`, the startup log includes `palette_fallback_notice: "16-color palette in use"`, and the status bar shows the same notice once on first render (`fr:11 §Terminal compatibility`). All glyphs used by the renderer (ack marker, in-flight indicator, etc.) are printable ASCII or BMP Unicode that ships in the macOS Terminal.app default font.

---

## 9. Manual refresh action

```rust
pub enum RefreshState {
    Idle,
    InFlight,
    DebouncedUntil(Instant),
}
```

Pressing `r`:

1. If `RefreshState::InFlight` → status bar shows `refresh: already in flight`. No new request.
2. If `RefreshState::DebouncedUntil(t)` and `Instant::now() < t` → status bar shows `refresh: debounced (<remaining>s)`. No request.
3. Else: state becomes `InFlight`. Spawn `tokio::spawn(client.post_refresh())`. The result task sends `Update::RefreshAck(RefreshAck)` and sets state to `DebouncedUntil(Instant::now() + 5s)`.

The 5-second floor matches the daemon-side coalescing cap (`fr:10 §POST /api/refresh` says bursts coalesce; the TUI does not pretend to know the exact daemon cadence — it shows the ack body's `coalesced` / `backoff_active` / `earliest_fire_at` fields verbatim in the status bar). Snapshot polls continue uninterrupted during an in-flight refresh.

---

## 10. Logging

A single structured JSON line emitted to **the TUI's own stderr** at startup (`fr:11 §Logging`):

```json
{
  "event": "roki_tui_started",
  "ts": "<rfc3339>",
  "api_url": "<url>",
  "polling": {
    "tickets_seconds": <u32>,
    "events_seconds": <u32>,
    "escalations_seconds": <u32>
  },
  "palette": "rgb24 | indexed_ansi256 | indexed_ansi16"
}
```

No further routine logging. Client errors that surface in the status bar are not duplicated to stderr (operators have the status bar; the daemon log is the authoritative trace). Schema-decode failures produce a single stderr line (event name, no body): `{"event":"roki_tui_decode_error","ts":"…","endpoint":"…","error":"…"}`. The TUI does not write to the daemon's log destination.

---

## 11. Spec impact

- `ref:cli` — new `roki-tui` section + new row in the top subcommand table mapping `roki-tui` → `fr:11`.
- `ref:log-events` — no new rows. The TUI's two stderr events are TUI-local and not part of the daemon's structured event catalog.
- `ref:config` — no rows. The TUI config file is independent of `roki.toml`.
- `roki-api-types::CycleSummary::last_state_id` added (additive). Slice-9 round-trip test extended.
- `roki-daemon::api::projection::cycles` populates `last_state_id` from `cycle.json::states.last()`. No new fixture; the existing `slice9-api-cycle-and-visit` assertion is extended.
- Workspace `Cargo.toml`: append `"crates/roki-tui"` to `[workspace.members]`.

No changes to `fr:10` or `fr:11` text.

---

## 12. Tests

### 12.1 Unit tests inside `roki-tui`

- `config` — defaults, CLI overrides, validation (rejected `tickets_seconds = 0`, malformed URL).
- `sanitize::ansi_strip` — CSI / OSC / SGR sequences, BEL terminator, lone `\x1b`, UTF-8-boundary safety.
- `sanitize::control_strip` — every C0 control except `\n` / `\t` removed; DEL removed; printable ASCII preserved.
- `sanitize::clean_json` — recursive descent into objects, arrays, nested arrays; non-string leaves untouched.
- `palette::detect` — table-driven with `$COLORTERM` / `$TERM` permutations via a `EnvProbe` trait so the test does not mutate the process env.
- `model::escalations::apply_escalations` — ack key retained when the matching entry persists; cleared when it disappears; new entries arrive un-acked.
- `model::events::merge_page` — strict monotonic `seq`; `gap: true` sets `gap_pending` until the next page's `seq` chain is contiguous.
- `client::*` — every method against `wiremock::MockServer` covering 2xx, 404, 5xx, malformed JSON, timeout.

### 12.2 Integration tests inside `roki-tui/tests/`

- `tui_smoke` — spawn a `wiremock::MockServer` returning canned `roki-api-types` JSON, point the TUI at it via the public `App::run_for_test(api_url, ticks)` entrypoint, drive 5 synthetic ticks, assert that the resulting `AppModel` snapshot has the expected ticket count / event seq / escalation count.
- `tui_refresh_ack` — drive the `r` key, assert that the mock recorded a single `POST /api/refresh` and that `AppModel::status` contains the ack text.
- `tui_palette_fallback` — set `$TERM=dumb`, assert the resolved palette is `IndexedAnsi16` and the startup log JSON line matches.

`App::run_for_test` is a feature-gated helper (`#[cfg(any(test, feature = "test-harness"))]`) that drives the model loop without a real `crossterm` terminal. It exists so the integration tests stay headless.

### 12.3 New e2e fixture inside `roki-daemon/tests/e2e/`

- `slice10-cycles-last-state-id` — drive a cycle to a terminal, `GET /api/tickets/{id}/cycles`, assert the response carries `last_state_id == terminal_id`. Then drive an in-flight cycle (suspend at a known state), assert `last_state_id` equals the in-flight state and `terminal_id` is absent.

No new daemon-side e2e for the TUI itself; the TUI integration tests cover the round trip against `wiremock`.

---

## 13. Implementation sequence

1. `roki-api-types`: add `CycleSummary::last_state_id`, round-trip test. `feat(slice10,api-types): add CycleSummary.last_state_id`.
2. `roki-daemon::api::projection::cycles`: populate `last_state_id` from `cycle.json::states.last()`. Extend `slice9-api-cycle-and-visit` assertions. `feat(slice10,daemon): project CycleSummary.last_state_id`.
3. New e2e fixture `slice10-cycles-last-state-id`. `test(slice10,api,e2e): add cycles_last_state_id_smoke`.
4. `crates/roki-tui` skeleton: Cargo manifest, `main.rs` with `eprintln!("hello")`, registered in workspace. Compiles + clippy clean. `feat(slice10,tui): scaffold crate`.
5. `cli.rs` (clap derive) + unit tests on argument parsing. `feat(slice10,tui): cli flags`.
6. `config.rs` (toml + `directories`) + unit tests on defaults / overrides / validation errors. `feat(slice10,tui): config loader`.
7. `sanitize.rs` + unit tests. `feat(slice10,tui): defense-in-depth sanitize`.
8. `palette.rs` + unit tests behind an `EnvProbe` trait. `feat(slice10,tui): palette detection`.
9. `client/*` (reqwest wrapper) + unit tests against `wiremock`. `feat(slice10,tui): http client`.
10. `model/*` (state + reducer functions, no I/O) + unit tests. `feat(slice10,tui): app model`.
11. `poll/*` (cadence loops sending Updates) + a small `tokio::time::pause`-based unit test on the scheduler. `feat(slice10,tui): poll scheduler`.
12. `ui/*` (ratatui widgets) + snapshot-style unit tests via `ratatui::buffer::Buffer` comparisons for the tickets and escalations tables. `feat(slice10,tui): render views`.
13. `input.rs` (crossterm key dispatch) + unit tests on the key-to-action mapping. `feat(slice10,tui): key bindings`.
14. `startup_log.rs` + integration into `main.rs`. `feat(slice10,tui): startup structured log`.
15. `App::run` orchestration in `main.rs` + `App::run_for_test` behind feature gate. `feat(slice10,tui): main run loop`.
16. Integration tests `tui_smoke`, `tui_refresh_ack`, `tui_palette_fallback`. Each its own commit. `test(slice10,tui): add <name>`.
17. Update `ref:cli` (new section + subcommand table row). `docs(slice10,ref:cli): add roki-tui section`.
18. Final sweep: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`, `kusara validate`. `chore(slice10): rustfmt + clippy clean`.

Each step is a separate commit on `feature/slice10-roki-tui`. Steps 1-3 land slice-9 follow-ups under `(slice10, …)` scope so the timeline reads cleanly.

---

## 14. Boundaries / non-goals

- **Daemon-side ack persistence** out of scope (`fr:11 §Boundaries`).
- **Persistent UI state across sessions** out of scope (`fr:11 §Boundaries`).
- **Mutating actions beyond refresh** out of scope (`fr:11 §Boundaries`).
- **State-transition diagram view** out of scope (`fr:11 §Boundaries`).
- **Web UI** out of scope (`fr:11 §Boundaries`).
- **Authentication / TLS** out of scope (`fr:11 §Boundaries`).
- **Windows support** out of scope (`fr:11 §Terminal compatibility`).
- **Sixel / Kitty graphics / advanced mouse tracking** out of scope (`fr:11 §Terminal compatibility`).
- **Embedded full-log viewer** out of scope (`fr:11 §Log inspection`); the TUI prints the matching `roki log` command line and the operator runs it.

---

## 15. Documented divergence

- **Refresh debounce floor** of 5 seconds is a new TUI-side constant. `fr:11 §Refresh action` says "respects the API-side minimum refresh interval; bursts are coalesced into a single in-flight request" without naming a number. The 5-second floor is conservative against the daemon's `[linear].polling.cadence_seconds` default of 300s — the daemon coalesces upstream regardless of how fast the TUI fires; the floor exists only to keep the local UI from queuing redundant nudges within a single key-bounce.
- **In-memory events cap** of 1000 rows in `EventsView::rows` is a new TUI-side constant. `fr:11 §Views` describes the events view as a "live tail of the event ring buffer" without naming a window; a 1000-row cap is the smallest value that still covers the daemon's default `[log].ring_size = 1000`.
- **Config-file path resolution** prefers the literal `~/.config/roki-tui/config.toml` documented in `fr:11 §Configuration` over the `directories`-derived macOS path. The directories-crate path is used only as a fallback so XDG-respecting users on Linux get the standard behavior without behavioural surprise.

---

## 16. Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-observability`; Constraints > Platform (terminal compatibility).
- **Requirements**:
  - `roki-observability Req 8` – `Req 11` (TUI binary, escalation ack, refresh action, terminal compatibility).
  - `roki-observability Req 6.4` (defense-in-depth sanitization on the TUI side).
  - `roki-observability Req 14.4` (TUI startup logging).
- **Related FR**: `fr:11-roki-tui` (primary), `fr:10-http-api` (consumed surface), `fr:08-observability-logs` (event stream), `fr:06-failure-handling` (escalation queue), `fr:09-log-access-cli` (`roki log` command-line hand-off).
