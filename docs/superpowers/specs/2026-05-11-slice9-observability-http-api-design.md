# Slice 9 — Observability HTTP API Design

Date: 2026-05-11
Scope: Implement `fr:10 §Endpoints` end to end — opt-in axum HTTP server backed by the diff cache, on-disk per-ticket captures, the `EscalationQueue` from slice 7, a new in-memory event ring (`fr:08 §Tier 3`), and a new Linear polling fallback / refresh-nudge tracker (`fr:03 §Polling fallback` and `§Refresh nudge`). Introduce the `roki-api-types` crate as the single schema source of truth, and add the `[api]` + `[linear].polling` `roki.toml` sections.

## 1. Position in the Roadmap

Slice 9 closes:

- `roki-http-server` — observability HTTP API per `fr:10`. Endpoints: `/api/healthz`, `/api/tickets`, `/api/tickets/{id}`, `/api/tickets/{id}/cycles`, `/api/tickets/{id}/cycles/{cycle_id}/visits/{n}/{state_id}/{stream}`, `/api/events`, `/api/escalations`, `POST /api/refresh`. Loopback default; default off; sanitized output; per-request structured log; `roki-api-types` shared crate.
- `roki-events-ring-buffer` — `fr:08 §Tier 3` in-memory ring buffer that backs `GET /api/events`. Each event tagged with a monotonic sequence number; ring sized by `[log].ring_size`; `gap: true` on `since=<seq>` older than the ring's oldest seq.
- `roki-tracker-polling-and-refresh-nudge` — `fr:03 §Polling fallback` cadence-bounded polling task and `§Refresh nudge` one-shot bumps that back `POST /api/refresh`. Sharing the existing `linear::rate_limit::RateLimitState` for cap + 429 backoff.
- `roki-api-types-crate` — new workspace member. Hosts every request / response / projection type referenced by the server, the daemon's projection pass, and (later) `roki-tui`.
- `roki-config-api-and-polling` — extend `RokiConfig` with `[api]` (gating, bind, port) and `[linear].polling` (`cadence_seconds`). Defaults match `ref:config`.

Slices 1–8 provide: cycle engine + state-machine driver, escalation queue, persistent dispatcher with diff cache, per-ticket session tempdir + cycle metadata files (`fr:09 §Storage layout`), structured event writer (per-ticket + `_daemon` scoped), admission filter, webhook receiver, cold-start enumeration, paginated Linear GraphQL primitive with rate-limit accounting.

Out of scope, deferred to later slices:

- **TUI** (`fr:11`). Slice 10. Consumes `roki-api-types` and the HTTP surface produced here.
- **`roki events --tail` / `roki log --follow`** (`fr:09`). Out of scope until slice 10. The CLI continues to read disk-side captures + the file destination directly; the new ring is not consumed by the CLI in this slice.
- **`/api/cycles/{cycle_id}` cross-ticket lookup**. `fr:10` only specifies cycle access scoped under a ticket id; preserve that scoping. A flat `/api/cycles/{id}` is a future extension.
- **WebSocket / SSE push** (`fr:10 §Boundaries`). Polling `since=<seq>` is the supported live-tail model.
- **Authentication / TLS / non-loopback bind hardening** (`fr:10 §Boundaries`). Loopback default + warn log on non-loopback bind only.
- **Hot reload of `[api].*` or `[linear].polling.*`** (`fr:10`). Restart-required.
- **`/api/refresh` body parameters** (filtering by ticket / repo). `fr:10` defines a body-less coalesced nudge only.
- **Persistent metrics / time-series** (`fr:10 §Boundaries`). Live snapshot only.

---

## 2. Architecture

### 2.1 Workspace + module layout

```
crates/
├── roki-api-types/               // NEW workspace member
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                // re-exports
│       ├── tickets.rs            // TicketSummary, TicketDetail, CycleSummary, VisitRef
│       ├── events.rs             // ApiEvent, EventsPage, GapMarker
│       ├── escalations.rs        // ApiEscalation
│       ├── healthz.rs            // Healthz
│       └── refresh.rs            // RefreshAck
└── roki-daemon/
    └── src/
        ├── api/                  // NEW
        │   ├── mod.rs            // ApiState handle, server::serve(), gate logic
        │   ├── config.rs         // ApiSection (loaded from RokiConfig)
        │   ├── routes.rs         // axum::Router build, per-endpoint handlers
        │   ├── projection/       // in-memory model → roki-api-types projection
        │   │   ├── mod.rs
        │   │   ├── tickets.rs
        │   │   ├── cycles.rs
        │   │   ├── visits.rs     // wraps cli::log storage abstraction
        │   │   ├── events.rs
        │   │   └── escalations.rs
        │   ├── sanitize.rs       // ansi_strip + html_escape; sanitize_failure replacement
        │   ├── log_layer.rs      // per-request middleware emitting api_request events
        │   └── healthz.rs        // request counter + uptime + repos
        ├── events.rs             // extend: ring buffer integration + Event::ApiRequest
        ├── observability/        // NEW
        │   └── ring.rs           // EventRing<seq=u64, capacity>
        ├── linear/
        │   ├── polling.rs        // NEW: PollingTracker task + RefreshNudge channel
        │   └── rate_limit.rs     // unchanged; reused
        ├── runtime.rs            // wire api::serve + ring + polling tracker before cold_start
        └── config/
            └── roki.rs           // ApiSection + LinearPollingSection wired into RokiConfig
```

### 2.2 `roki-api-types` crate

- Edition: workspace.
- Dependencies: `serde = { workspace = true, features = ["derive"] }`, `serde_json = workspace`, `time = { workspace, features = ["serde-well-known"] }`, `uuid = { workspace, features = ["serde", "v4"] }`. **No** `tokio`, `axum`, or `roki-daemon` dependency.
- Every type derives `Debug, Clone, Serialize, Deserialize, PartialEq` (the last for round-trip tests). Optional fields are `Option<T>` with `#[serde(skip_serializing_if = "Option::is_none")]`.

Representative shapes (full field set is doc-commented in each module):

```rust
// tickets
pub struct TicketSummary {
    pub ticket_id: String,
    pub repo: String,
    pub status: String,
    pub labels: Vec<String>,
    pub assignee: String,
    pub in_flight_cycle_id: Option<Uuid>,
    pub last_event_at: OffsetDateTime,
}

pub struct TicketDetail {
    #[serde(flatten)]
    pub summary: TicketSummary,
    pub recent_events: Vec<ApiEvent>,    // bounded; capacity = roki.toml [api].ticket_events_window
    pub truncated: bool,
}

pub struct CycleSummary {
    pub cycle_id: Uuid,
    pub kind: CycleKind,                 // enum: Rule, Cleanup, Failure
    pub trigger: CycleTrigger,           // enum: Runtime, ColdStart
    pub started_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
    pub terminal_id: Option<String>,
    pub failure_kind: Option<String>,    // canonical lowercase string from FailureKind
    pub total_visits: u32,
}

// events
pub struct ApiEvent {
    pub seq: u64,
    pub ts: OffsetDateTime,
    pub event: String,                   // canonical event name
    pub ticket_id: Option<String>,
    pub cycle_id: Option<Uuid>,
    pub payload: serde_json::Value,      // post-sanitization JSON object
}

pub struct EventsPage {
    pub events: Vec<ApiEvent>,
    pub gap: bool,
    pub next_since: Option<u64>,
}

// escalations
pub struct ApiEscalation {
    pub ticket_id: Option<String>,
    pub cycle_id: Option<Uuid>,
    pub kind: String,                    // failure kind canonical string
    pub state_id: Option<String>,
    pub visit_n: Option<u32>,
    pub timestamp: OffsetDateTime,
    pub error_text: String,
    pub marker: String,                  // FailureMarker canonical string
}

// healthz
pub struct Healthz {
    pub version: String,                 // env!("CARGO_PKG_VERSION")
    pub uptime_seconds: u64,
    pub configured_repositories: Vec<String>,
    pub api_request_count: u64,
}

// refresh
pub struct RefreshAck {
    pub coalesced: bool,
    pub earliest_fire_at: Option<OffsetDateTime>,
    pub backoff_active: bool,
}
```

Forward-compat fallback strings (per `fr:10 §Read-only projection`): `CycleKind::Failure` for unknown server-side kinds, `failure_kind = "unknown"` for unknown failure kinds. The mapping logic lives in `roki-daemon/src/api/projection/*` so new server-side kinds are handled in the projection layer without touching the types crate.

### 2.3 Event ring buffer

```rust
// observability::ring

pub struct EventRing {
    inner: Mutex<RingInner>,
    capacity: usize,
}

struct RingInner {
    seq_counter: u64,
    buf: VecDeque<RingEntry>,            // capacity = self.capacity
}

struct RingEntry {
    seq: u64,
    ts: OffsetDateTime,
    event_kind: String,
    ticket_id: Option<String>,
    cycle_id: Option<Uuid>,
    payload: serde_json::Value,
}

impl EventRing {
    pub fn new(capacity: usize) -> Arc<Self>;

    /// Record an event. Returns the assigned monotonic sequence number.
    pub fn record(
        &self,
        event_kind: &str,
        ticket_id: Option<&str>,
        cycle_id: Option<Uuid>,
        payload: serde_json::Value,
    ) -> u64;

    /// Range query for `GET /api/events`.
    /// `since` is the last seq the client observed; the response begins at
    /// `since + 1`. When `since < oldest_seq()` the page carries `gap: true`
    /// and starts from `oldest_seq()`.
    /// Filters AND together. `limit` caps page size.
    pub fn page(
        &self,
        since: Option<u64>,
        kind: Option<&str>,
        ticket: Option<&str>,
        cycle: Option<Uuid>,
        limit: usize,
    ) -> EventsPage;

    pub fn oldest_seq(&self) -> Option<u64>;
    pub fn newest_seq(&self) -> Option<u64>;
}
```

Wiring contract:
- `events.rs` gains an `Arc<EventRing>` next to the `EventWriter`. Every `EventWriter::emit` call funnels through a thin helper that (a) writes to the underlying tracing-crate JSON Lines destination AND (b) records into the ring with a serialized payload. Sanitization of agent / Linear strings happens in `EventWriter::emit` (already present per slice 8 sanitization paths) so the ring stores the same sanitized form the file destination sees.
- `[log].ring_size = 0` disables the ring. `EventRing::record` becomes a no-op; `page` returns `EventsPage { events: [], gap: false, next_since: None }`. Operators that disable the ring see `GET /api/events` return an empty array; `since` parameters older than the current seq counter still report `gap: true`.

### 2.4 Polling tracker + refresh nudge

```rust
// linear::polling

pub struct PollingTracker {
    cfg: Arc<RokiConfig>,
    workflow: Arc<WorkflowConfig>,
    rate_limit: Arc<linear::rate_limit::RateLimitState>,
    graphql: Arc<LinearGraphqlClient>,
    dispatcher: Arc<Dispatcher<RealCycleRunner>>,
    cache: Arc<DiffCache>,
    nudge_rx: Mutex<mpsc::Receiver<NudgeRequest>>,
    nudge_tx_handle: NudgeHandle,
    daemon_writer: Arc<Mutex<EventWriter>>,
}

#[derive(Clone)]
pub struct NudgeHandle {
    tx: mpsc::Sender<NudgeRequest>,
}

struct NudgeRequest {
    requested_at: Instant,
    ack: oneshot::Sender<RefreshAck>,
}

impl PollingTracker {
    pub fn spawn(...) -> NudgeHandle;        // spawns the loop; returns the handle

    /// One iteration of the loop, exposed for unit tests with a mock clock.
    async fn tick(&self) -> TickOutcome;     // TickOutcome::Polled / Skipped / BackoffActive
}

impl NudgeHandle {
    /// Awaits the tracker's coalescing decision and returns it. Used by
    /// POST /api/refresh.
    pub async fn nudge(&self) -> RefreshAck;
}
```

Cadence and coalescing rules (`fr:03 §Polling fallback` + `§Refresh nudge`):

1. The loop sleeps until `next_fire = max(last_fire + cadence, rate_limit.next_legal())`.
2. On wake the tracker drains the nudge channel. Empty drain → outage-driven tick (only if webhook delivery has been silent for `cadence`); non-empty drain → nudge-driven tick. Both call the same enumerate path as cold start with the current status union.
3. **Coalescing**: every nudge that arrives between `last_fire` and `next_fire` is acknowledged with `coalesced: true` and the same `earliest_fire_at`. A single tick fires at `next_fire` and clears them all.
4. **429 backoff active** (`rate_limit.in_backoff()` true) → the nudge is acknowledged with `backoff_active: true, coalesced: false, earliest_fire_at: rate_limit.next_legal()`; no tick is scheduled (the request is dropped, not queued, per `fr:10 §POST /api/refresh`).
5. **Outage detection**: tracker watches the `WebhookReceiver`'s last-success timestamp via a shared `AtomicI64` ms-since-epoch. If `now - last_webhook_success > 2 * cadence`, the tracker fires an outage tick on its next wake even if the nudge channel is empty. Successful webhooks zero the timer; the tracker then idles.
6. **Cap**: the tracker never schedules a fire earlier than `last_fire + cadence`; nudges arriving inside that window are coalesced, never honored sooner.
7. **Logging** (info severity): each tick logs one structured event `polling_tick { trigger: outage | nudge, status_set, enumerated, admitted }`. Each acknowledged nudge logs `refresh_nudge_acknowledged { coalesced, backoff_active, client_addr }` from the HTTP layer (so the polling task itself does not see the client address).

### 2.5 HTTP server module

```rust
// api

pub struct ApiState {
    cache: Arc<DiffCache>,
    workflow: Arc<WorkflowConfig>,
    cfg: Arc<RokiConfig>,
    escalation: Arc<EscalationQueue>,
    ring: Arc<EventRing>,
    nudge: NudgeHandle,
    request_counter: AtomicU64,
    boot_time: OffsetDateTime,
}

pub async fn serve(state: Arc<ApiState>, daemon_writer: Arc<Mutex<EventWriter>>) -> Result<(), ApiBindError>;
```

Routing table (`api/routes.rs`):

| Method | Path | Handler |
|---|---|---|
| GET | `/api/healthz` | `healthz::get` |
| GET | `/api/tickets` | `projection::tickets::list` |
| GET | `/api/tickets/{id}` | `projection::tickets::detail` |
| GET | `/api/tickets/{id}/cycles` | `projection::cycles::list` |
| GET | `/api/tickets/{id}/cycles/{cycle_id}/visits/{n}/{state_id}/{stream}` | `projection::visits::stream` |
| GET | `/api/events` | `projection::events::page` |
| GET | `/api/escalations` | `projection::escalations::list` |
| POST | `/api/refresh` | `projection::refresh::post` |

Headers (every JSON response): `Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`. 404 / 4xx bodies use the same envelope `{ "error": "<canonical_code>", "detail": "<human_readable>" }`.

Path param validation rules:
- `{id}` (ticket id): `[A-Za-z0-9_-]{1,64}`. Reject with 400 + `error: "invalid_ticket_id"` on miss.
- `{cycle_id}`: parse via `Uuid::parse_str`. Reject with 400 + `error: "invalid_cycle_id"` on miss.
- `{n}`: `u32` (decimal). Reject with 400 + `error: "invalid_visit_n"`.
- `{state_id}`: `[A-Za-z0-9_-]{1,64}` AND must appear in the cycle's recorded state ids. Mismatch → 404 + `error: "state_id_not_found_in_cycle"`.
- `{stream}`: enum `stdout | stderr | directive | terminal | events | exit_code`. Mismatch → 400 + `error: "invalid_stream"`.

`/api/events` query parameters: `since` (`u64`), `kind` (string), `ticket` (string), `cycle` (uuid), `limit` (default 200, max 1000).

`/api/tickets/{id}/cycles` reads cycle metadata from `<session_root>/<ticket>/cycle-<uuid>/cycle.json` (a new metadata file, see §3). For each `cycle.json` the projection layer loads a `CycleSummary`. If the file is missing the cycle is omitted from the response.

### 2.6 Sanitization

```rust
// api::sanitize

/// Strips ANSI escape sequences and HTML-escapes the result. Used on
/// every projection field that originates from a Linear payload or a
/// state subprocess.
pub fn clean_text(s: &str) -> String;

/// Same as `clean_text`, plus replaces invalid UTF-8 with U+FFFD and
/// returns `Some(field_name)` to be logged when a replacement happens.
pub fn clean_text_or_placeholder(field_name: &str, raw: &[u8]) -> (String, Option<&'static str>);

/// Apply `clean_text` to every string leaf in a JSON value. Used on the
/// ring's payload field before it lands in `ApiEvent`.
pub fn clean_json(value: &mut serde_json::Value);
```

`clean_text` uses the `vte` crate (already in the workspace via `tokio-util` if available, else add a direct dependency) or a hand-rolled state machine over the byte stream — design choice resolved at implementation time. HTML escape uses `html_escape::encode_text`. Sanitization happens in the projection layer so the in-memory model stays raw.

### 2.7 Per-request structured log

Custom axum middleware (`api/log_layer.rs`):

1. Increments `ApiState::request_counter` on every request.
2. On the response future's completion emits an `api_request` event (added to the catalog in §6.1) with: `method`, `path` (raw, without query), `query` (sanitized: keys preserved, values redacted to `*` to avoid leaking ticket ids in logs), `status`, `duration_ms`, `client_addr`, `correlation_id` (UUID v4 generated per request).
3. Bodies are not captured. Successful 2xx responses still emit the event; 5xx additionally emit at warn severity.
4. The middleware never consumes the response body.

Secret redaction reuses `events::redact`; no per-API-layer redactor.

### 2.8 Configuration

`RokiConfig` extension (`config::roki`):

```rust
pub struct RokiConfig {
    // ... existing fields ...
    pub api: ApiSection,
    pub linear: LinearSection,            // gains `pub polling: PollingSection`
}

pub struct ApiSection {
    pub bind: String,                     // default "127.0.0.1"
    pub port: Option<u16>,                // None disables the server
    pub ticket_events_window: u32,        // default 50; min 1, max 500
    pub cycle_list_window: u32,           // default 50; min 1, max 500
}

pub struct PollingSection {
    pub cadence_seconds: u32,             // default 300; min 60
}
```

Validation (refuses startup):
- `[api].port = 0` → `invalid_port_zero`.
- `[api].bind` not a valid IP literal → `invalid_bind_addr`.
- `[api].ticket_events_window` outside 1..=500 → `invalid_window`.
- `[api].cycle_list_window` outside 1..=500 → `invalid_window`.
- `[linear].polling.cadence_seconds` < 60 → `invalid_cadence`.

`ref:config` already documents `[api].port`, `[api].bind`, `[linear].polling.cadence_seconds`. New row: `[api].ticket_events_window`. Update `ref:config` in slice scope.

### 2.9 Wiring at boot

`runtime::run_inner` order (post-slice-9):

1. Load `RokiConfig`.
2. Load `WorkflowConfig`.
3. Construct `EventRing` with capacity `cfg.log.ring_size`.
4. Construct `EscalationQueue` (slice 7, unchanged).
5. Construct `EventWriter` over destination + ring + escalation queue (`record` path now drives both file and ring).
6. Construct `Dispatcher` (slice 5, unchanged).
7. Spawn `PollingTracker` → returns `NudgeHandle`.
8. Spawn `WebhookReceiver` (slice 1, unchanged) sharing the rate-limit + last-webhook-success atom with the tracker.
9. Run `ColdStart` (slice 6, unchanged).
10. Emit `daemon_ready`.
11. **If `cfg.api.port.is_some()`**: build `ApiState`, call `api::serve`, spawn into the runtime. On bind failure log `api_bind_failed` and continue.

Shutdown (`fr:12`): SIGTERM drains the dispatcher first, then the polling tracker stops accepting nudges (returns `coalesced: true, earliest_fire_at: None, backoff_active: false`), then the API server stops accepting connections. Deadline `[engine].shutdown_window_seconds`.

---

## 3. Cycle metadata file (new on-disk artifact)

`fr:10 §GET /api/tickets/{id}/cycles` requires per-cycle summary data. Existing on-disk layout has `<session_root>/<ticket>/cycle-<uuid>/visit-<n>/...` per `fr:09`, but no per-cycle metadata file. Add:

```
<session_root>/<ticket>/cycle-<uuid>/cycle.json
```

Content:

```json
{
  "cycle_id": "<uuid>",
  "ticket_id": "<id>",
  "kind": "rule | cleanup | failure",
  "trigger": "runtime | cold_start",
  "started_at": "<rfc3339>",
  "ended_at": "<rfc3339> | null",
  "terminal_id": "<state_id> | null",
  "failure_kind": "<canonical> | null",
  "total_visits": <u32>,
  "states": ["<state_id>", ...]
}
```

Written by `daemon::ticket_task` at cycle start (with `ended_at: null`) and updated atomically (`<path>.tmp` + rename) at cycle end. On crash the file may stay open-ended; the projection layer treats `ended_at: null` as "in flight" and reports `failure_kind: null`. `ref:artifacts` row added for `cycle.json`.

`states[]` lets the visit endpoint validate `{state_id}` against the cycle without scanning visit dirs.

---

## 4. Event additions

`events.rs` extends the `Event` enum:

```rust
pub enum Event {
    // ... existing ...
    ApiRequest {
        ts: OffsetDateTime,
        method: String,
        path: String,
        query_keys: Vec<String>,             // value-redacted
        status: u16,
        duration_ms: u32,
        client_addr: String,
        correlation_id: String,
    },
    ApiBindFailed {
        ts: OffsetDateTime,
        bind: String,
        port: u16,
        error: String,
    },
    ApiDisabled {
        ts: OffsetDateTime,
    },
    PollingTick {
        ts: OffsetDateTime,
        trigger: String,                     // "outage" | "nudge"
        status_set: Vec<String>,
        enumerated: u32,
        admitted: u32,
    },
    RefreshNudgeAcknowledged {
        ts: OffsetDateTime,
        coalesced: bool,
        backoff_active: bool,
        client_addr: String,
    },
}
```

`ref:log-events` rows added for each. `fr:08 §Event catalog` updated.

---

## 5. Endpoint behaviour details

### 5.1 `/api/healthz`

Body:

```json
{
  "version": "<crate version>",
  "uptime_seconds": <u64>,
  "configured_repositories": ["<ghq>", ...],
  "api_request_count": <u64>
}
```

`configured_repositories` is the union of `admission.repos[].ghq` from the top-level workflow file and from every loaded per-repo override file (preserves slice 8 semantics where overrides expand the configured set).

### 5.2 `/api/tickets`, `/api/tickets/{id}`

`list` returns a snapshot of every entry currently in `DiffCache` (slice 5). The cache lock is held only for the duration of the snapshot construction. Sort: `last_event_at` descending.

`detail` augments the summary with the most recent `[api].ticket_events_window` entries from the ring filtered by `ticket_id == {id}`. `truncated = true` when the ring has more matching entries than the window.

### 5.3 `/api/tickets/{id}/cycles`

Lists `cycle.json` files under `<session_root>/<ticket>`. Sort: `started_at` descending. Bound: at most `[api].cycle_list_window` entries (default 50, validated 1..=500). Past that bound truncate; response carries `truncated: true`.

### 5.4 Visit stream

Maps to `cli::log` storage (slice 1). The `{stream}` enum maps to file paths under `visit-<n>/`:

| `{stream}` | File |
|---|---|
| `stdout` | `<state_id>.stdout` |
| `stderr` | `<state_id>.stderr` |
| `directive` | `<state_id>.directive.json` |
| `terminal` | `<state_id>.terminal.json` |
| `events` | `<state_id>.events.jsonl` (if present) |
| `exit_code` | `<state_id>.exit_code` |

Response:
- Stdout / stderr / exit_code → `text/plain; charset=utf-8`, ANSI-stripped, NO HTML escape (terminal logs are not rendered as HTML by clients in this slice; the TUI stripping in slice 10 is sufficient).
- Directive / terminal → `application/json; charset=utf-8`. JSON parsed + re-serialized with HTML escape on string leaves.
- `events` (per-state events.jsonl) → `application/x-ndjson; charset=utf-8`. Each line is parsed + sanitized (HTML escape + ANSI strip on string leaves) and re-serialized; non-JSON-parsing lines are dropped with a `dropped_lines` count emitted as a `Roki-Dropped-Lines` response header.
- File missing → 404 + `error: "stream_not_found"`.

Streaming: bodies are read fully into memory (capture files are bounded by the existing per-state cap; no streaming complexity for slice 9). A future slice can switch to `tokio_util::io::ReaderStream`.

### 5.5 `/api/events`

Calls `EventRing::page` with parsed query parameters. Default `limit = 200`, max 1000.

### 5.6 `/api/escalations`

Calls `EscalationQueue::snapshot` (slice 7). Each entry passes through `projection::escalations::project`:

- `error_text` is sanitized via `clean_text`.
- `kind` → `FailureKind` canonical lowercase string.
- `marker` → `FailureMarker` canonical lowercase string.

### 5.7 `POST /api/refresh`

Body: ignored (any content type accepted). Response: `RefreshAck` from `NudgeHandle::nudge`.

---

## 6. Tests

### 6.1 New e2e fixtures (`crates/roki-daemon/tests/e2e/`)

- `slice9-api-disabled` — `[api].port` unset; daemon emits `api_disabled`; `curl :nope` skipped (no port). Verifies the gate.
- `slice9-api-healthz` — port set; `GET /api/healthz` returns 200 with `version`, `configured_repositories` matching admission, `api_request_count >= 1`.
- `slice9-api-tickets-list` — admit two tickets via webhook; `GET /api/tickets` returns 2 entries; sanitization assertion: a label containing `<script>` and an ANSI escape comes back HTML-escaped + ANSI-stripped.
- `slice9-api-cycle-and-visit` — webhook → cycle runs → `GET /api/tickets/{id}/cycles` returns the cycle id; `GET /api/.../visits/1/<state_id>/stdout` returns the captured `out`.
- `slice9-api-events-since` — drive several events; `GET /api/events?since=0` returns ordered seq numbers; `since=<old>` after ring rotation returns `gap: true`.
- `slice9-api-escalations` — recursion-bound a handler cycle (slice 7 + slice 8 fixture pattern); `GET /api/escalations` returns 1 entry with the right `kind` + `state_id`.
- `slice9-api-refresh-coalesce` — two nudges fired concurrently; both ack with `coalesced: true` against the same `earliest_fire_at`; one polling_tick event emitted on the daemon log.
- `slice9-api-refresh-backoff` — inject a 429 response into the wiremock; `POST /api/refresh` returns `backoff_active: true`; no `polling_tick` emitted.
- `slice9-api-non-loopback-warn` — `[api].bind = "0.0.0.0"`; daemon emits a warn-severity log naming the bind host; server still binds.
- `slice9-api-bind-failure` — pre-bind the chosen port from the test process; daemon emits `api_bind_failed`; daemon continues to `daemon_ready`; webhook still works.

Each fixture follows the slice-8 e2e pattern (inline support helpers + `support_cold_start`).

### 6.2 Updated existing e2e

- `skeleton_smoke` and other slice-1-7 fixtures get `[api]` omitted from their `roki.toml` literals — no behavioural change required because the section is optional. Done as a sweep in the final task.

### 6.3 Unit tests

- `roki-api-types` — round-trip `serde_json::to_string` then `from_str` for every type; assert canonical field names.
- `observability::ring` — `record` + `page` ordering, `gap` semantics, `kind` filter, `since=None` returns from the start.
- `linear::polling` — `tick` outage trigger, nudge coalescing, 429 backoff drop.
- `api::sanitize` — ANSI strip, HTML escape, JSON leaf walking, invalid UTF-8 placeholder.
- `api::projection::*` — projection of a hand-built `DiffCache` + `EscalationQueue` + `EventRing` snapshot.
- `api::log_layer` — counter increments, `api_request` payload shape (mock event writer).
- `api::routes` — every endpoint with a fully constructed `ApiState` (axum `oneshot` against the router); 4xx envelopes; 404 envelope.

---

## 7. Implementation sequence

1. Workspace + crate skeleton: add `crates/roki-api-types`, declare types with stub bodies, register in `[workspace.members]`. Compiles + clippy clean.
2. Define every type fully + round-trip serde tests in `roki-api-types`.
3. `RokiConfig` extension: `ApiSection` + `LinearPollingSection`. Defaults + validation + unit tests. Update `ref:config` row + add `[api].ticket_events_window` row.
4. `observability::ring` module + unit tests. Wire into `EventWriter::emit`.
5. `linear::polling` module + unit tests. No wiring yet.
6. `api/sanitize.rs` + unit tests.
7. `api/projection/*` modules building `roki-api-types` from in-memory state + on-disk capture. Unit tests against synthetic state.
8. `api/log_layer.rs` middleware + unit test.
9. `api/routes.rs` axum router + handlers. Unit tests via `tower::ServiceExt::oneshot`.
10. `api::serve` boot path + bind-failure handling. Wire into `runtime::run_inner`.
11. `daemon::ticket_task` writes `cycle.json` at cycle start + atomically updates at cycle end. Update `ref:artifacts`.
12. Spawn `PollingTracker` in `runtime::run_inner` + share rate-limit + webhook-success atom with the receiver.
13. Add `Event::{ApiRequest, ApiBindFailed, ApiDisabled, PollingTick, RefreshNudgeAcknowledged}`. Update `ref:log-events` + `fr:08 §Event catalog`.
14. e2e fixtures (§6.1). Each one is a separate commit.
15. Sweep: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`, `kusara validate`.

Each step is a separate commit on `feature/slice9-observability-http-api`. Steps 1-13 can land as `feat(slice9,api): ...` commits; step 14 produces 10 `test(slice9,api,e2e): ...` commits; step 15 produces a final `chore(slice9): rustfmt + clippy clean`.

---

## 8. Boundaries / non-goals

- **Authentication, TLS, CORS** out of scope (`fr:10 §Boundaries`).
- **WebSocket / SSE** out of scope; live tail = poll `since`.
- **Hot reload** of `[api].*` or `[linear].polling.*` out of scope.
- **Cross-ticket cycle lookup** (`/api/cycles/{cycle_id}` without ticket id) out of scope.
- **`/api/tickets` filtering / pagination** out of scope; full snapshot only. The cache size is bounded by admission.
- **Per-state-id stream live-tail** out of scope; a single read.
- **`/api/refresh` body parameters** out of scope.
- **TUI consumption** out of scope (slice 10).

---

## 9. Documented divergence

- **`/api/tickets/{id}/cycles`** introduces a new on-disk artifact (`cycle.json`). `fr:09 §Storage layout` and `ref:artifacts` will gain a row in slice scope. The artifact is an additive sibling of the existing `visit-<n>/` directories; it does not move or rename existing files.
- **Visit-stream content type** for `stdout` / `stderr` / `exit_code` is `text/plain` rather than `application/json`. `fr:10` does not pin a content type for these streams; this matches operator expectations (raw subprocess output) and avoids JSON-wrapping a binary-safe byte stream.
- **`PollingSection`** sits under `[linear]` (matching `ref:config` `[linear].polling.cadence_seconds`) rather than its own top-level `[polling]` block.
- **Outage detection threshold** of `2 * cadence` since the last webhook success is a new operational constant introduced here. `fr:03 §Polling fallback` describes outage-driven polling without specifying a threshold; `2 * cadence` is the smallest value that gives webhook delivery a fair chance to recover before the daemon makes the operator pay an extra Linear API call.
