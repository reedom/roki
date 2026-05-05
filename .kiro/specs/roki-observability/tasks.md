---
refs:
  id: tasks:roki-observability
  kind: tasks
  title: "roki-observability Tasks"
  spec: roki-observability
  depends_on:
    - design:roki-observability
---

# Implementation Plan

> **Path note:** the daemon crate lives at `crates/roki-daemon/`, not at repo root. Every path below is relative to repo root. The Cargo workspace already exists (`Cargo.toml` declares `[workspace] resolver = "3"` with members `crates/roki-daemon` and `crates/roki-doctools`); section 1 only adds new members.

## 0. Preconditions: roki-mvp surface gaps to close before this spec can implement

The current `OrchestratorRead` / `IssueState` surface in `crates/roki-daemon/src/orchestrator/read.rs` is too thin for Requirement 3.1 (per-issue detail) and the snapshot fields enumerated in Requirement 2.2. These tasks extend roki-mvp's already-published surface in an additive way (no shape changes to existing fields) and MUST land before tasks 2.4–2.5 of this spec. They are listed here rather than as a roki-mvp follow-up because they exist solely to support roki-observability and the design's "Revalidation Triggers" already names additions to projection types as a trigger.

- [ ] 0.1 Extend `IssueState` (and `ActorSnapshot`) projection with the fields Req 2.2 / 3.1 require
  - In `crates/roki-daemon/src/orchestrator/read.rs`, extend `IssueState` with: `repo: Option<String>`, `workspace_path: Option<PathBuf>`, `permission_strategy: Option<String>`, `last_event: Option<TransitionEventSummary>`, `last_event_at: Option<DateTime<Utc>>`, `last_error: Option<String>`, `correlation_id: Option<String>`. Mirror the same fields on `ActorSnapshot` and update the orchestrator construction site that fills `ActorSnapshot`.
  - Define `TransitionEventSummary { previous: WorkerState, next: WorkerState, trigger: TransitionTrigger, at: DateTime<Utc> }` adjacent to `TransitionEvent` in `crates/roki-daemon/src/orchestrator/state.rs` (or in `read.rs`; pick whichever has lower coupling).
  - All new fields are `Option`; existing call sites that construct `ActorSnapshot` may pass `None` — additive only.
  - Observable completion: `cargo test -p roki-daemon` passes; an existing roki-mvp test asserts each pre-existing field still round-trips; a new test asserts `IssueState` exposes the seven new fields.
  - _Requirements: 2.2, 3.1, 13.3_
  - _Boundary: crates/roki-daemon/src/orchestrator/read.rs, crates/roki-daemon/src/orchestrator/state.rs_

- [ ] 0.2 Resolve `SnapshotResponse` name collision
  - `crates/roki-daemon/src/orchestrator/read.rs` defines an internal `SnapshotResponse` that collides with `roki_api_types::SnapshotResponse` once the new crate is imported. Rename the internal type to `OrchestratorSnapshot` (referenced by that name in this spec's design.md) and update all call sites in roki-daemon.
  - Observable completion: `cargo check --workspace` succeeds with no name collision; every reference within `crates/roki-daemon` to the old `read::SnapshotResponse` updated.
  - _Requirements: 12.1, 12.5_
  - _Boundary: crates/roki-daemon/src/orchestrator/read.rs (+ call sites)_

- [ ] 0.3 Expose `SubscriberHooks` from the orchestrator construction surface
  - Design.md claimed `OrchestratorRead::subscribe(...)`, but subscribe lives on `SubscriberHooks` (`crates/roki-daemon/src/orchestrator/hooks.rs`). Confirm `SubscriberHooks` is reachable from the daemon main task (it already is via the orchestrator builder), and document the call sequence in a doc comment on `SubscriberHooks::subscribe`: "register before serving". No code change required if the handle is already public; otherwise re-export it under `crate::orchestrator::SubscriberHooks`.
  - Observable completion: a doctest on `SubscriberHooks::subscribe` shows a minimal registration example; `cargo test --doc -p roki-daemon` passes.
  - _Requirements: 13.1, 13.5_
  - _Boundary: crates/roki-daemon/src/orchestrator/hooks.rs_

## 1. Workspace and shared API types: prepare new members and the schema crate

- [ ] 1.1 Add `roki-api-types` and `roki-tui` as new workspace members
  - The workspace already exists (`Cargo.toml`: `[workspace] resolver = "3"`, members `crates/roki-daemon`, `crates/roki-doctools`). Append two new members: `crates/roki-api-types` and `crates/roki-tui`.
  - Create `crates/roki-api-types/Cargo.toml` with `edition = "2024"` and dependencies `serde`, `serde_json`, `chrono`.
  - Create `crates/roki-tui/Cargo.toml` with `edition = "2024"` and dependencies `ratatui`, `crossterm`, `reqwest`, `clap`, `serde`, `serde_json`, `tracing`, `strip-ansi-escapes`, plus a path dep on `roki-api-types`.
  - Add `roki-api-types = { path = "../roki-api-types" }` as a path dependency to `crates/roki-daemon/Cargo.toml`.
  - Observable completion: `cargo check --workspace` succeeds with all four crates compiling; `crates/roki-daemon` can `use roki_api_types::*`.
  - _Requirements: 12.1, 12.2, 12.3_
  - _Boundary: Cargo.toml, crates/roki-api-types/Cargo.toml, crates/roki-tui/Cargo.toml, crates/roki-daemon/Cargo.toml_

- [ ] 1.2 Define the shared `serde` types in `roki-api-types`
  - Implement `SnapshotResponse`, `WorkerSummary`, `RepoSummary`, `EscalationEntry`, `AggregateUsage`, `AggregateRateLimit`, `DaemonInfo`, `ServerBlock`, `RecentEventSummary` under `crates/roki-api-types/src/snapshot.rs`.
  - `WorkerSummary { repo: String, issue: String, state: String, inactive_reason: Option<String>, last_event: Option<RecentEventSummary>, last_event_at: Option<DateTime<Utc>>, correlation_id: Option<String> }`. `inactive_reason` is set when `state == "inactive"` (mirrors `fr:04-state-machine-and-recovery` `Inactive(reason=...)` discriminator).
  - `RecentEventSummary { kind: String, message: Option<String>, at: DateTime<Utc>, correlation_id: Option<String> }` — `kind` is the snake_case discriminator of the `TransitionTrigger` (e.g. `phase_event`, `tracker_event`, `orchestrator_action`, `assignment_lost`, `roki_ready_removed`); `message` carries any escaped string preview.
  - `ServerBlock { bind: String, port: u16, request_counters: BTreeMap<String, u64>, min_refresh_interval_seconds: u64, max_event_log_per_issue: u64 }` — TUI reads `min_refresh_interval_seconds` from here for client-side debounce (Req 10.4); `request_counters` satisfies Req 14.5.
  - `EscalationEntry { repo: String, issue: String, failure_reason: String, raised_at: DateTime<Utc>, correlation_id: Option<String> }` — `failure_reason` is the snake_case discriminator from `fr:14-operator-notifications`.
  - Implement `IssueDetailResponse`, `RecentEventEntry`, `ProjectionState` under `crates/roki-api-types/src/issue.rs`. `IssueDetailResponse` includes `inactive_reason: Option<String>` with the same semantics as `WorkerSummary`.
  - Implement `RefreshRequest` (reserved empty in v1) and `RefreshAccepted` under `crates/roki-api-types/src/refresh.rs`.
  - Implement `ApiError`, `ApiErrorCode` under `crates/roki-api-types/src/error.rs`.
  - Add `pub const API_VERSION: &str = "v1";` and re-exports in `lib.rs`.
  - Document each struct with a doc comment describing each field's purpose, including roki-specific shape decisions (multi-repo `(repo, issue)` keying as separate `repo`/`issue` fields, `inactive_reason`, escalation entry shape).
  - Observable completion: a unit test in `roki-api-types` round-trips one fully populated `SnapshotResponse` (containing one worker with `state == "inactive"` + `inactive_reason == Some("orchestrator_crash")`, one `EscalationEntry`, and a populated `ServerBlock.request_counters`) and one `IssueDetailResponse` through `serde_json` without loss; a second test asserts `inactive_reason` serializes to `null` when `None`; a third test asserts every response shape's `api_version` equals `API_VERSION`; `cargo test -p roki-api-types` passes.
  - _Requirements: 2.2, 3.1, 5.1, 5.2, 5.3, 5.4, 5.5, 12.1, 12.2, 12.3, 12.4, 12.5, 14.5_
  - _Boundary: crates/roki-api-types_

- [ ] 1.3 (P) Document the `/api/v1/*` contract in `SPEC.md`
  - Add a top-level `## Observability HTTP API` section to `SPEC.md` describing the three endpoints, status codes, response headers (`Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`), per-field documentation of every response shape from `roki-api-types`, the version-stability rule (additions are additive only within `/api/v1/`; breaking changes go to `/api/v2/`), the security note that loopback is the default and any non-loopback bind is unauthenticated, and a `### Snapshot timing and bounds` subsection that pins the documented numerics: snapshot drift bound = 50ms (Req 2.3), TUI startup window = 1s on a loopback developer-class machine (Req 8.2), `min_refresh_interval_seconds` default = 5 (Req 4.4, 15.2), `max_event_log_per_issue` default = 50 (Req 3.6, 15.2), TUI pre-first-fetch debounce default = 1000ms (Req 10.4).
  - Cross-reference the `server.*` config block (added by task 2.2).
  - Observable completion: `SPEC.md` contains the three endpoint subsections with status codes, a per-field table for each response shape, the version-stability paragraph, the loopback security note, and the `### Snapshot timing and bounds` subsection with the five numerics; a grep for `/api/v1/state`, `/api/v1/refresh`, `api_version`, `loopback`, `50ms`, and `min_refresh_interval_seconds` in `SPEC.md` returns the new content.
  - _Requirements: 5.4, 7.3_
  - _Boundary: SPEC.md_

- [ ] 1.4 (P) Add the example `server.*` block to `WORKFLOW.example.md`
  - Append a commented-out `server` block under the existing `extension` section with `port`, `bind`, `min_refresh_interval_seconds`, `max_event_log_per_issue` and an inline comment that names the absent-authn risk. If `WORKFLOW.example.md` lacks an `extension:` section, add a top-level commented-out `extension:` parent above the `server` block.
  - Observable completion: a unit test that parses `WORKFLOW.example.md` confirms the file still validates against the existing schema (the new block is commented), and a manual grep for `server:` and `127.0.0.1` in `WORKFLOW.example.md` shows the example.
  - _Requirements: 7.3, 15.5_
  - _Boundary: WORKFLOW.example.md_

## 2. Server module: configuration, projection, and routing

- [ ] 2.1 Implement `escape.rs`: HTML-escape plus ANSI-strip helper
  - Create `crates/roki-daemon/src/server/escape.rs` with `pub fn apply(input: &str) -> String` that strips ANSI escape sequences using `strip-ansi-escapes` then HTML-escapes via `html-escape`, returning a fixed sanitized placeholder for invalid UTF-8 input.
  - Emit a structured tracing event (level warn) when the placeholder fires, naming the source field via an `apply_with_field(field: &str, input: &str) -> String` variant.
  - Observable completion: a unit test asserts `apply("\x1b[31m<b>hi</b>")` returns `"&lt;b&gt;hi&lt;/b&gt;"`, that a malformed input returns `"[redacted-invalid]"`, and that the `apply_with_field` variant emits a tracing event with the field name (captured via `tracing-test`).
  - _Requirements: 2.4, 3.4, 6.1, 6.2, 6.3, 6.5_
  - _Boundary: crates/roki-daemon/src/server/escape.rs_

- [ ] 2.2 Implement `config.rs`: parse and validate the `server.*` block
  - Create `crates/roki-daemon/src/server/config.rs` with `ServerConfig`, `ServerConfigError`, and `pub fn from_policy(policy: &WorkflowPolicy) -> Result<Option<ServerConfig>, ServerConfigError>`.
  - Read the block from `WorkflowPolicy::raw_unknowns["extension"]["server"]` — the `extension.server.*` namespace is already reserved by roki-mvp's `WorkflowPolicy` (per `crates/roki-daemon/src/workflow/schema.rs` doc comment "extension.server.*"). No new schema registration is needed; the loader already round-trips this blob verbatim.
  - Validate `port` (1..=65535), `bind` (parse as `IpAddr`, default `127.0.0.1`), `min_refresh_interval_seconds` (>=1, default 5), `max_event_log_per_issue` (>=1, default 50).
  - Add `pub fn is_loopback(&self) -> bool` that returns true when `bind` falls within `127.0.0.0/8` or `::1/128`.
  - Observable completion: unit tests show that a missing `server` block parses to `Ok(None)`, a valid block parses to `Ok(Some(ServerConfig { ... }))` with documented defaults, an out-of-range `port` returns `Err(InvalidPort)`, and a malformed `bind` returns `Err(InvalidBind)`; a hot-reload negative-path test feeds an invalid `server.port` to a running daemon (via `crates/roki-daemon/src/workflow/watcher.rs` reload path) and asserts the previous policy remains in effect, an error log names the offending key, and the daemon does not exit (per Req 15.3a + `fr:02-configuration`); a hot-reload positive-path test feeds a valid `server.port` change and asserts the loader stores the new policy but the listening socket does NOT rebind at runtime (Req 1.6) — a structured info log records the deferred-until-restart decision.
  - _Requirements: 1.4, 1.5, 7.1, 7.2, 15.1, 15.2, 15.3, 15.3a, 15.4_
  - _Boundary: crates/roki-daemon/src/server/config.rs_

- [ ] 2.3 Implement `escalation_queue_view.rs`: read-only view of roki-mvp's `EscalationQueue`
  - roki-mvp already owns `crate::orchestrator::escalation::EscalationQueue` and exposes it via `OrchestratorRead::escalation_queue() -> Vec<EscalationEntry>` (in roki-mvp's internal shape `{issue: IssueId, repo: Option<String>, kind: EscalationKind, correlation_id: String, timestamp, structured_fields}`). This task does NOT create a parallel queue. Instead, create `crates/roki-daemon/src/server/escalation_queue_view.rs` with a small mapper `pub fn to_api_entry(e: &orchestrator::escalation::EscalationEntry) -> roki_api_types::EscalationEntry` that maps `EscalationKind` to the snake_case `failure_reason` discriminator from `fr:14-operator-notifications` (`orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`, `stall`, `retry_exhausted`, `fs_poison`, `orphan`, `allowlist_rejected`, `needs_split`, `spec_incomplete`, `needs_operator`).
  - Map `repo: Option<String>` to `String` by emitting `""` (empty repo) when absent — the API shape is `String` per `roki_api_types::EscalationEntry` and consumers treat empty as "pre-worktree / unknown repo".
  - If `EscalationKind` does not yet have a 1:1 mapping for every reason listed above, extend `EscalationKind` (in roki-mvp) additively under task 0.x rather than silently dropping cases — flag any gap in this task's PR description.
  - Observable completion: unit tests assert: (a) every `EscalationKind` variant maps to a non-empty `failure_reason` string; (b) `to_api_entry` round-trips `correlation_id` and `timestamp` (renamed `raised_at` on the API side); (c) a fixture covering every daemon-only failure reason from `fr:14` is accepted.
  - _Requirements: 2.1, 13.1, 13.2_
  - _Boundary: crates/roki-daemon/src/server/escalation_queue_view.rs_
  - _Depends: 0.1, 0.2, 1.2_

- [ ] 2.4 Implement `event_cache.rs` and the `TransitionSubscriberAdapter`
  - Create `crates/roki-daemon/src/server/event_cache.rs` with `EventCache` storing one bounded `VecDeque<RecentEventEntry>` per `IssueId`, with capacity from `ServerConfig.max_event_log_per_issue`. **Scope: feeds the per-issue detail endpoint only — does NOT contribute to `SnapshotResponse.escalations` (escalations come from `OrchestratorRead::escalation_queue()` via 2.3).** Keying is `IssueId` alone because 1 ticket = 1 repo per `roki-mvp` design.md.
  - Implement `EventCache::push(&self, key: &IssueId, entry: RecentEventEntry)` and `EventCache::recent(&self, key: &IssueId) -> (Vec<RecentEventEntry>, bool /* truncated */)`.
  - Implement `TransitionSubscriberAdapter` that holds an `Arc<EventCache>` and implements roki-mvp's `TransitionSubscriber::on_transition(&self, event: &TransitionEvent)`. The trait method is **synchronous and non-blocking** (per `crates/roki-daemon/src/orchestrator/hooks.rs:28`) — the adapter MUST NOT call `.await`, MUST NOT block on locks, and MUST isolate panics (the dispatcher already wraps in `catch_unwind`, but adapter implementation should still avoid panicking paths). The trait does NOT have a `veto` method; do not add one.
  - The adapter writes one `RecentEventEntry` to `EventCache` per transition. It does NOT write to the escalation queue: roki-mvp's orchestrator already maintains `EscalationQueue` on transitions into `Inactive(reason=...)` (per existing roki-mvp design). The observability spec only reads through `OrchestratorRead::escalation_queue()`.
  - On terminal-state transition (`next == Cleaning` or removal), schedule a grace-window drop of the cache entry via `tokio::spawn` + `tokio::time::sleep(Duration::from_secs(2))` so that one final snapshot can still surface the terminal event before the entry is purged.
  - Observable completion: a unit test pushes ten entries against a cache with capacity five and asserts `recent` returns five entries with `truncated == true`; a second test confirms the adapter's `on_transition` completes synchronously (no `.await`) under `tokio::time::pause()` and writes the entry; a third test feeds a terminal transition, advances mock time by 2 seconds, and asserts the entry is purged.
  - _Requirements: 2.2, 3.1, 3.6, 13.1, 13.2_
  - _Boundary: crates/roki-daemon/src/server/event_cache.rs_

- [ ] 2.5 Implement `projection.rs`: assemble snapshot and per-issue responses
  - Create `crates/roki-daemon/src/server/projection.rs` with `Projection` holding `Arc<dyn OrchestratorRead>`, `Arc<EventCache>`, and `Arc<HttpRequestCounter>` (the counter handle from 2.7).
  - Implement `pub fn build_snapshot(&self) -> Result<roki_api_types::SnapshotResponse, ProjectionError>` and `pub fn build_issue_detail(&self, issue: &str) -> Result<roki_api_types::IssueDetailResponse, ProjectionError>`. Disambiguate the name collision by importing `roki_api_types::SnapshotResponse as ApiSnapshot` if needed (after 0.2 the internal type is `OrchestratorSnapshot`, so the collision is gone — but the alias keeps the import explicit).
  - Map roki-mvp's `WorkerState` per `fr:04-state-machine-and-recovery` to `ProjectionState` strings: `Pending → "pending"`, `Active → "active"`, `Backoff → "backoff"`, `Inactive(_) → "inactive"`, `Cleaning → "cleaning"`. (Verified: these are the five canonical variants in `crates/roki-daemon/src/orchestrator/state.rs:17`.) Any future variant maps to `"unknown"` with the original variant name surfaced in the response `details` field.
  - When `WorkerState == Inactive(reason)`, populate `WorkerSummary.inactive_reason` and `IssueDetailResponse.inactive_reason` with the `InactiveReason` discriminator as snake_case; leave `None` for every other state.
  - Build `SnapshotResponse.escalations` from `self.orchestrator.escalation_queue()` mapped through `escalation_queue_view::to_api_entry` (do NOT derive escalations from `EventCache`).
  - The "latest lifecycle event" surfaced via `WorkerSummary.last_event` is read from `IssueState.last_event` populated by orchestrator (per 0.1); the projection observes only the resulting transition, never directly subscribing to phase-subprocess exit envelopes (`fr:18-worker-skill-workflow`) or orchestrator-stdin daemon directives (`fr:19-orchestrator-session`).
  - For per-issue lookup, call `OrchestratorRead::issue(&IssueId::from(issue))`. Map `None` → `ProjectionError::NotFound`, `Some(_)` → projected `IssueDetailResponse`. There is no multi-repo disambiguation: one ticket maps to at most one repo per `roki-mvp` design.md (multi-repo tickets are rejected by the orchestrator with `outcome=needs_split`); `IssueId` alone is unique.
  - Apply `escape::apply_with_field` to every agent-derived and Linear-derived string field (issue title, description, last-event message, last-error string, label values, tool-result preview).
  - Build `SnapshotResponse.server` with `request_counters` from the shared `HttpRequestCounter`, plus `min_refresh_interval_seconds` and `max_event_log_per_issue` echoed from `ServerConfig`, and the bound `bind` / `port`.
  - Return `ProjectionError::Unhealthy` (mapped later to 503), `ProjectionError::AmbiguousRepo` (400), and `ProjectionError::NotFound` (404).
  - Snapshot drift bound: perform the three sub-reads — (1) `OrchestratorRead::snapshot()`, (2) per-issue event cache reads, (3) `OrchestratorRead::escalation_queue()` — in that fixed order, then stamp `snapshot_at = Utc::now()`. The bound is ≤50ms (Req 2.3); test is in 4.1.
  - Observable completion: a unit test using a stub `OrchestratorRead`, stub `EventCache`, and stub `HttpRequestCounter` produces a `SnapshotResponse` whose every string field is HTML-escaped and ANSI-stripped (verified by injecting `\x1b[31m<b>` test fixtures); a worker in `Inactive(orchestrator_crash)` state surfaces with `state == "inactive"` + `inactive_reason == Some("orchestrator_crash")` and a matching `EscalationEntry`; a worker in `Inactive(awaiting_linear)` surfaces with `inactive_reason == Some("awaiting_linear")` but does NOT appear in the escalations array (it is not a daemon-only failure per fr:14); ambiguous-repo and not-found cases produce the correct error variant; the response's `api_version` equals `roki_api_types::API_VERSION`.
  - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.6, 3.1, 3.2, 3.3, 3.4, 5.1, 5.2, 5.5, 13.2, 13.3, 13.4_
  - _Boundary: crates/roki-daemon/src/server/projection.rs_
  - _Depends: 0.1, 0.2, 2.1, 2.3, 2.4, 2.7_

- [ ] 2.6 Implement `refresh.rs`: refresh debounce and tracker nudge
  - Create `crates/roki-daemon/src/server/refresh.rs` with `RefreshCoordinator` holding `Arc<dyn TrackerRefresh>`, a `Mutex<RefreshDebounceState>`, and the configured `min_refresh_interval`.
  - Implement `pub async fn request(&self, client: SocketAddr) -> RefreshAccepted` that returns `coalesced: true` when a previous request lies within the minimum interval, otherwise calls `tracker.nudge()` and returns the tracker-reported `earliest_fire_at`.
  - Log every request at info level with the client address and the coalescing decision.
  - Observable completion: a unit test against a fake `TrackerRefresh` issues three back-to-back calls with a one-second minimum interval and asserts the second and third return `coalesced: true` while the first returns `coalesced: false`; the test also asserts `tracker.nudge()` was called exactly once.
  - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_
  - _Boundary: crates/roki-daemon/src/server/refresh.rs_

- [ ] 2.7 Implement `logging.rs`: per-request tracing layer and request counter
  - Create `crates/roki-daemon/src/server/logging.rs` with a tower middleware that emits a structured `tracing::info!` event per request with `method`, `path`, `status`, `duration_ms`, `client_addr`, `correlation_id`, never including the response body.
  - Define `pub struct HttpRequestCounter(Arc<DashMap<&'static str, AtomicU64>>)` with `pub fn increment(&self, endpoint: &'static str)` and `pub fn snapshot(&self) -> BTreeMap<String, u64>`. The middleware increments per-endpoint counts; `Projection` (2.5) reads the snapshot for `SnapshotResponse.server.request_counters`.
  - Reuse the daemon's existing redaction layer (no new redaction logic).
  - Observable completion: a unit test issues three requests against an in-memory router, captures tracing events with `tracing-test`, and asserts each event contains the documented fields and that `HttpRequestCounter::snapshot` reflects three increments on the matching endpoint.
  - _Requirements: 14.1, 14.2, 14.3, 14.5_
  - _Boundary: crates/roki-daemon/src/server/logging.rs_

- [ ] 2.8 Implement `router.rs`: axum routes for the three endpoints
  - Create `crates/roki-daemon/src/server/router.rs` exposing `pub fn build(state: Arc<AppState>) -> axum::Router`.
  - Wire `GET /api/v1/state` to `Projection::build_snapshot`, `GET /api/v1/<issue>` to `Projection::build_issue_detail`, `POST /api/v1/refresh` to `RefreshCoordinator::request`. The `<issue>` path takes no query parameters; an unknown query parameter must result in a 400 `BAD_REQUEST` (the API does not silently accept extras, since multi-repo disambiguation is explicitly out of scope).
  - Map `ProjectionError::Unhealthy -> 503`, `NotFound -> 404`, and any uncaught error to 500 `INTERNAL` with redaction.
  - Set `Content-Type: application/json; charset=utf-8` and `Cache-Control: no-store` on every JSON response via tower middleware.
  - Attach the logging middleware from task 2.7.
  - Observable completion: an integration test in `crates/roki-daemon/tests/integration_server_router.rs` runs the router against a stub `OrchestratorRead` and asserts each endpoint returns the documented status, the documented headers, and a body that round-trips through `serde_json` to `SnapshotResponse` / `IssueDetailResponse` / `RefreshAccepted`.
  - _Requirements: 2.1, 2.5, 2.6, 3.1, 3.2, 3.3, 3.5, 3.6, 4.1, 4.2_
  - _Boundary: crates/roki-daemon/src/server/router.rs_
  - _Depends: 2.5, 2.6, 2.7_

- [ ] 2.9 Implement `mod.rs`: optional `ServerModule` lifecycle and bind
  - Create `crates/roki-daemon/src/server/mod.rs` with `ServerModule` holding `ServerConfig`, `Arc<dyn OrchestratorRead>`, `Arc<dyn TrackerRefresh>`, `Arc<EventCache>`, `Arc<HttpRequestCounter>`, `Arc<SubscriberHooks>`.
  - Implement `pub async fn run(self, shutdown: ShutdownSignal) -> Result<(), ServerError>` that constructs `EventCache`, wires it into a `TransitionSubscriberAdapter`, registers the adapter via `SubscriberHooks::subscribe(...)` before binding the socket, binds via `axum::serve(TcpListener::bind((self.config.bind, self.config.port)))`, and uses `with_graceful_shutdown(shutdown.fut())`.
  - On non-loopback bind, emit a warn-level tracing event before serving begins; on bind failure, emit an error-level event and return `Ok(())` (no retry, daemon continues without the API).
  - Add a top-level helper `pub fn maybe_spawn(...) -> Option<JoinHandle<()>>` that returns `None` when `ServerConfig::from_policy` returned `Ok(None)`, logs an info-level "API disabled" event in that case, and otherwise spawns the server.
  - Observable completion: an integration test starts the daemon with a `WORKFLOW.md` containing a valid `server.port`, hits `GET /api/v1/state`, observes a 200 response, then sends shutdown and observes a clean exit; a second test starts the daemon with no `server` block and confirms no port is bound and the "API disabled" log fires once; a third test injects a daemon-only failure transition and asserts the resulting `GET /api/v1/state` body contains the matching `EscalationEntry`.
  - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 7.1, 7.2, 13.1, 13.5, 14.4_
  - _Boundary: crates/roki-daemon/src/server/mod.rs_
  - _Depends: 0.4, 2.2, 2.3, 2.4, 2.8_

- [ ] 2.10 Wire the server module into `crates/roki-daemon/src/main.rs` daemon startup
  - In `crates/roki-daemon/src/main.rs`, after the workflow loader reports a valid policy and after the orchestrator and tracker are constructed, call `server::maybe_spawn(&policy, orchestrator.clone(), tracker.clone(), subscriber_hooks.clone(), shutdown.clone())`.
  - Ensure the daemon continues running normally when `maybe_spawn` returns `None` and that shutdown propagates to the spawned task when `Some`.
  - Observable completion: an integration test starts the daemon end-to-end with both API-enabled and API-disabled `WORKFLOW.md` fixtures and asserts the orchestrator runs in both cases; the API-enabled case also passes a TUI-style health-check (`GET /api/v1/state` returns 200).
  - _Requirements: 1.1, 1.2, 13.5_
  - _Boundary: crates/roki-daemon/src/main.rs_
  - _Depends: 2.9_

## 3. TUI binary: rendering, input, and refresh loop

- [ ] 3.1 (P) Implement `terminal_caps.rs`: capability detection and palette fallback
  - Create `crates/roki-tui/src/terminal_caps.rs` with `pub fn detect() -> TerminalCaps` returning `{ truecolor: bool, fallback_notice: Option<String> }`.
  - Probe `COLORTERM` (`truecolor`/`24bit`), `TERM_PROGRAM`, and crossterm's reported color depth; on Windows return a marker that triggers a top-level not-supported exit.
  - Observable completion: unit tests with controlled environment variables show `detect` returning truecolor when `COLORTERM=truecolor`, returning the fallback notice when unset, and surfacing the Windows marker on `cfg(target_os = "windows")` (compile-time gated).
  - _Requirements: 11.1, 11.2, 11.5_
  - _Boundary: crates/roki-tui/src/terminal_caps.rs_

- [ ] 3.2 (P) Implement `sanitize.rs`: defense-in-depth ANSI strip and control-char filter
  - Create `crates/roki-tui/src/sanitize.rs` with `pub fn safe(input: &str) -> String` that ANSI-strips and removes ASCII control characters except newline and tab.
  - Observable completion: a unit test asserts `safe("\x1b[31mhi\x07")` returns `"hi"` and `safe("ok\nyes")` returns `"ok\nyes"`.
  - _Requirements: 6.4_
  - _Boundary: crates/roki-tui/src/sanitize.rs_

- [ ] 3.3 (P) Implement `api_client.rs`: thin reqwest wrapper
  - Create `crates/roki-tui/src/api_client.rs` with `TuiApiClient` holding a `reqwest::Client` and a base URL.
  - Implement `async fn state(&self) -> Result<SnapshotResponse, TuiApiError>`, `async fn refresh(&self) -> Result<RefreshAccepted, TuiApiError>`, and `async fn issue(&self, issue: &str) -> Result<IssueDetailResponse, TuiApiError>`.
  - Map non-2xx into `TuiApiError::Status { code, message }`; never panic on bad bodies.
  - Observable completion: a unit test against a wiremock or `httptest` fixture asserts each method deserializes the documented schema correctly and that a 503 produces `TuiApiError::Status { code: 503, .. }`.
  - _Requirements: 8.2, 8.3, 8.6, 10.1, 10.3_
  - _Boundary: crates/roki-tui/src/api_client.rs_
  - _Depends: 1.2_

- [ ] 3.4 Implement `render.rs`: layout, color palette, glyph table
  - Create `crates/roki-tui/src/render.rs` with `pub fn frame(state: &AppState, caps: &TerminalCaps, frame: &mut ratatui::Frame)`.
  - Lay out the screen as: header (daemon, uptime, aggregate usage, aggregate rate-limit) / split (workers table left, escalations right) / status bar.
  - Distinguish acknowledged escalations from unacknowledged via both color and a non-color glyph (`*` unacked, `.` acked) so Terminal.app fallback still differentiates.
  - Render only printable ASCII or commonly supported Unicode glyphs.
  - Observable completion: a unit test using `ratatui::backend::TestBackend` renders a known `AppState` and asserts the resulting buffer contains the expected glyphs at the documented coordinates and that the fallback notice appears on the status bar when `caps.fallback_notice.is_some()`.
  - _Requirements: 8.4, 9.2, 11.1, 11.2, 11.4_
  - _Boundary: crates/roki-tui/src/render.rs_
  - _Depends: 3.1_

- [ ] 3.5 Implement `input.rs`: key handling, refresh debounce, ack and quit keys
  - Create `crates/roki-tui/src/input.rs` with `pub enum InputAction { Refresh, ToggleAck, Quit, None }` and `pub fn map_event(event: crossterm::event::Event) -> InputAction`.
  - Implement a refresh debounce that ignores `Refresh` actions issued within the active `min_refresh_interval`. The interval is supplied by the caller (3.6) as a `Duration` argument; the input layer itself does not own the value.
  - Observable completion: a unit test feeds a sequence of synthetic key events and asserts the documented `InputAction` mapping; a separate test asserts the debounce drops the second `Refresh` action issued 100 ms after the first when the minimum interval is 1000 ms.
  - _Requirements: 8.5, 9.1, 10.1, 10.4_
  - _Boundary: crates/roki-tui/src/input.rs_

- [ ] 3.6 Implement `app.rs`: app loop, refresh task, channels, `AppState`
  - Create `crates/roki-tui/src/app.rs` with `AppState` holding the latest `SnapshotResponse`, `last_error: Option<String>`, `acked_escalations: HashSet<String>`, `refresh_in_flight: bool`, `last_refresh_at: Option<Instant>`, `effective_min_refresh: Duration`.
  - **Debounce interval source** (Req 10.4): `effective_min_refresh` starts at the documented pre-first-fetch default of `Duration::from_millis(1000)`. After each successful `state()` fetch, update `effective_min_refresh = Duration::from_secs(snapshot.server.min_refresh_interval_seconds)`. The input layer's debounce check reads this value via the shared `AppState`.
  - Drive the app loop: spawn a refresh task that calls `TuiApiClient::state` at the configured cadence, drain crossterm events, merge updates via `tokio::sync::mpsc` channels, render each tick with `TuiRender::frame`.
  - Clear acknowledgement entries when their underlying escalation is no longer present in the snapshot.
  - On `InputAction::Quit`, restore terminal mode (leave raw mode, show cursor) and exit zero.
  - On `InputAction::Refresh`, call `TuiApiClient::refresh` non-blocking and surface the result in the status bar.
  - Run every incoming string through `sanitize::safe` before storing it in `AppState` (defense in depth even though the server already escapes).
  - Observable completion: an integration test in `crates/roki-tui/tests/app_loop.rs` runs the app loop against a stub HTTP server and asserts: an initial frame renders the workers table, a `Refresh` key triggers exactly one POST, a non-2xx response surfaces in the status bar, a successful snapshot updates `effective_min_refresh` to the server-reported value, and a `Quit` key restores terminal mode and exits.
  - _Requirements: 8.2, 8.3, 8.4, 8.5, 9.1, 9.2, 9.3, 9.4, 10.1, 10.2, 10.3, 10.4_
  - _Boundary: crates/roki-tui/src/app.rs_
  - _Depends: 3.2, 3.3, 3.4, 3.5_

- [ ] 3.7 Implement `main.rs`: CLI, runtime bootstrap, terminal mode setup, Windows guard
  - Create `crates/roki-tui/src/main.rs` with a clap CLI accepting `--url <URL>` (required), `--refresh-interval-ms <MS>` (optional, default 1000), `--quit-key <KEY>` (optional, default `q`).
  - Bootstrap a tokio runtime, call `terminal_caps::detect`, exit non-zero with a not-supported message on Windows, otherwise enter raw mode, hide the cursor, run `app::run(...)`, and on any exit restore the terminal cleanly.
  - Emit one structured tracing startup event (to stderr only, not the daemon log) naming the API URL and the refresh cadence.
  - Observable completion: `cargo run -p roki-tui -- --help` prints the documented flags; a smoke test runs the binary against a stub loopback server, asserts the first frame is rendered within the documented startup window of 1 second (Req 8.2), verifies the startup log line is present on stderr (and asserts it is NOT written to any file under the daemon log directory, Req 14.4), and confirms the binary exits zero on `Quit`.
  - _Requirements: 8.1, 8.2, 8.5, 11.5, 14.4_
  - _Boundary: crates/roki-tui/src/main.rs_
  - _Depends: 3.6_

## 4. Integration and end-to-end tests across the daemon plus TUI

- [ ] 4.1 Integration test: `GET /api/v1/state` end-to-end with stub orchestrator
  - Add `crates/roki-daemon/tests/integration_server_state.rs` that spawns a daemon-shaped harness with a stub `OrchestratorRead`, a stub `EventCache` pre-populated with three issues including one with `\x1b[31m<b>boom</b>` in its last-event message, and a stub `TrackerRefresh`.
  - Assert: 200 status, `Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`, the response body deserializes back into `SnapshotResponse` cleanly, the response's `api_version` equals `roki_api_types::API_VERSION` (Req 5.5), and the offending agent-derived field is `&lt;b&gt;boom&lt;/b&gt;` (escaped, stripped).
  - **Snapshot drift bound**: a second test in the same file pre-populates the stub `OrchestratorRead` with 100 active issues, drives a background task that fires concurrent transitions for the duration of the snapshot call, and asserts the wall-clock between the first sub-read (`OrchestratorRead::snapshot`) and the third sub-read (escalation queue) stays at or under 50ms (the documented snapshot drift bound from Req 2.3).
  - Observable completion: `cargo test --test integration_server_state -p roki-daemon` passes both tests.
  - _Requirements: 2.1, 2.3, 2.4, 2.5, 5.1, 5.5, 6.1, 6.2, 6.3_
  - _Boundary: crates/roki-daemon/tests/integration_server_state.rs_
  - _Depends: 2.8, 2.9_

- [ ] 4.2 Integration test: `GET /api/v1/<issue>` happy and not-found paths
  - Add `crates/roki-daemon/tests/integration_server_issue.rs` that exercises the per-issue endpoint with two fixtures: an existing issue (200) and an unknown issue (404 `NOT_FOUND`). No multi-repo ambiguity case: 1 ticket = 1 repo per `roki-mvp` design.md.
  - Assert each response body matches the documented shape, the documented HTTP status codes, and that every response (including errors) carries `api_version == roki_api_types::API_VERSION` (Req 5.5).
  - Observable completion: `cargo test --test integration_server_issue -p roki-daemon` passes.
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 5.5_
  - _Boundary: crates/roki-daemon/tests/integration_server_issue.rs_
  - _Depends: 2.8, 2.9_

- [ ] 4.3 Integration test: `POST /api/v1/refresh` debounce against fake tracker
  - Add `crates/roki-daemon/tests/integration_server_refresh.rs` that issues three back-to-back POSTs against a fake `TrackerRefresh` whose nudge succeeds and then a fourth after the minimum interval has elapsed.
  - Assert: the first and fourth POSTs return `coalesced: false`, the second and third return `coalesced: true`, and the fake tracker's `nudge` counter is exactly 2.
  - Observable completion: `cargo test --test integration_server_refresh -p roki-daemon` passes.
  - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_
  - _Boundary: crates/roki-daemon/tests/integration_server_refresh.rs_
  - _Depends: 2.8, 2.9_

- [ ] 4.4 Integration test: WORKFLOW.md-gated server start and loopback warning
  - Add `crates/roki-daemon/tests/integration_server_gating.rs` exercising three startup paths: (a) no `server` block, observe API-disabled log and no listening port; (b) `server.port: <free port>` with no `bind`, observe the loopback bind, info-level "API listening on 127.0.0.1:<port>" log (assertion uses regex match on the port number, not exact string), and a successful `GET /api/v1/state`; (c) `server.bind: 0.0.0.0`, observe a warn-level non-loopback log fires once at startup.
  - Observable completion: `cargo test --test integration_server_gating -p roki-daemon` passes; `tracing-test` captures show the documented info and warn events.
  - _Requirements: 1.1, 1.2, 1.4, 1.5, 7.1, 7.2_
  - _Boundary: crates/roki-daemon/tests/integration_server_gating.rs_
  - _Depends: 2.9, 2.10_

- [ ] 4.5 E2E test: TUI happy path against running daemon harness
  - Add `crates/roki-tui/tests/tui_e2e.rs` that spawns the daemon harness from task 4.1 and runs `roki-tui` against it via `ratatui::backend::TestBackend`, asserting the initial frame contains the active worker table, that a refresh key triggers one `POST /api/v1/refresh`, that a non-2xx response surfaces in the status bar, and that the quit key exits zero.
  - Observable completion: `cargo test -p roki-tui --test tui_e2e` passes.
  - _Requirements: 8.2, 8.3, 8.4, 8.5, 10.1, 10.2, 10.3_
  - _Boundary: crates/roki-tui/tests/tui_e2e.rs_
  - _Depends: 3.7, 4.1_

- [ ] 4.6 E2E test: TUI degraded-terminal path and acknowledgement clearing
  - Extend the TUI E2E suite with two additional cases: one that injects a `TerminalCaps { truecolor: false, fallback_notice: Some(...) }` and asserts the status bar contains the one-time fallback notice; another that pushes an escalation, acks it, then removes it from the snapshot and asserts the acknowledgement state is cleared from `AppState.acked_escalations`.
  - Observable completion: both cases pass under `cargo test -p roki-tui --test tui_e2e`.
  - _Requirements: 9.1, 9.3, 11.2, 11.4_
  - _Boundary: crates/roki-tui/tests/tui_e2e.rs_
  - _Depends: 4.5_

- [ ] 4.7 Cross-spec consistency check: run roki-mvp tests with API enabled and disabled
  - Add a smoke test target `crates/roki-daemon/tests/integration_observability_no_regression.rs` that runs the daemon end-to-end with one API-enabled `WORKFLOW.md` fixture and one API-disabled fixture, exercising the same orchestrator-level scenarios that already pass in roki-mvp's test suite (workspace creation, transition events, recovery), and asserts they still pass in both configurations.
  - Observable completion: `cargo test --test integration_observability_no_regression -p roki-daemon` passes; the test confirms that enabling the API does not change orchestrator behavior or break any existing mvp-level invariant.
  - _Requirements: 13.1, 13.5_
  - _Boundary: crates/roki-daemon/tests/integration_observability_no_regression.rs_
  - _Depends: 2.10_
