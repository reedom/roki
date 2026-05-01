# Implementation Plan

- [ ] 1. Workspace and shared API types: prepare the multi-crate layout and the schema crate

- [ ] 1.1 Convert the daemon crate into a Cargo workspace and add new members
  - Update the root `Cargo.toml` to declare a `[workspace]` with `members = [".", "crates/roki-api-types", "crates/roki-tui"]` and a `resolver = "2"`.
  - Create `crates/roki-api-types/Cargo.toml` and `crates/roki-tui/Cargo.toml` with `edition = "2024"` and the documented dependencies (serde, serde_json, chrono for `roki-api-types`; ratatui, crossterm, reqwest, clap, serde, serde_json, tracing, strip-ansi-escapes for `roki-tui`).
  - Add `roki-api-types = { path = "crates/roki-api-types" }` as a path dependency to the daemon crate.
  - Observable completion: `cargo check --workspace` succeeds with all three crates compiling and the daemon crate able to import `roki_api_types`.
  - _Requirements: 12.1, 12.2, 12.3_
  - _Boundary: workspace_

- [ ] 1.2 Define the shared `serde` types in `roki-api-types`
  - Implement the `SnapshotResponse`, `WorkerSummary`, `RepoSummary`, `EscalationEntry`, `AggregateUsage`, `AggregateRateLimit`, `DaemonInfo`, `ServerBlock`, `RecentEventSummary` types under `crates/roki-api-types/src/snapshot.rs`.
  - Implement `IssueDetailResponse`, `RecentEventEntry`, `ProjectionState` under `crates/roki-api-types/src/issue.rs`.
  - Implement `RefreshRequest` (reserved empty in v1) and `RefreshAccepted` under `crates/roki-api-types/src/refresh.rs`.
  - Implement `ApiError`, `ApiErrorCode` under `crates/roki-api-types/src/error.rs`.
  - Add `pub const API_VERSION: &str = "v1";` and re-exports in `lib.rs`.
  - Document each struct with a doc comment that names the symphony field it mirrors and any roki-only addition (e.g. multi-repo `(repo, issue)` keying).
  - Observable completion: a unit test in `roki-api-types` round-trips one fully populated `SnapshotResponse` and one `IssueDetailResponse` through `serde_json` without loss; `cargo test -p roki-api-types` passes.
  - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 12.1, 12.2, 12.3, 12.4, 12.5_
  - _Boundary: roki-api-types_

- [ ] 1.3 (P) Document the `/api/v1/*` contract in `SPEC.md`
  - Add a top-level `## Observability HTTP API` section to `SPEC.md` describing the three endpoints, status codes, response headers (`Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`), the symphony-compatibility note, and the security note that loopback is the default and any non-loopback bind is unauthenticated.
  - Cross-reference the `server.*` config block (added by task 2.2).
  - Observable completion: `SPEC.md` contains the three endpoint subsections with status codes and one paragraph each on symphony compatibility and the loopback security note; a grep for `/api/v1/state`, `/api/v1/refresh`, `loopback`, and `symphony` in `SPEC.md` returns the new content.
  - _Requirements: 5.4, 7.3_
  - _Boundary: SPEC.md_

- [ ] 1.4 (P) Add the example `server.*` block to `WORKFLOW.example.md`
  - Append a commented-out `server` block under the existing `extension` section with `port`, `bind`, `min_refresh_interval_seconds`, `max_event_log_per_issue` and an inline comment that names the absent-authn risk.
  - Observable completion: a unit test that parses `WORKFLOW.example.md` confirms the file still validates against the existing schema (the new block is commented), and a manual grep for `server:` and `127.0.0.1` in `WORKFLOW.example.md` shows the example.
  - _Requirements: 7.3, 15.5_
  - _Boundary: WORKFLOW.example.md_

- [ ] 2. Server module: configuration, projection, and routing

- [ ] 2.1 Implement `escape.rs`: HTML-escape plus ANSI-strip helper
  - Create `src/server/escape.rs` with `pub fn apply(input: &str) -> String` that strips ANSI escape sequences using `strip-ansi-escapes` then HTML-escapes via `html-escape`, returning a fixed sanitized placeholder for invalid UTF-8 input.
  - Emit a structured tracing event (level warn) when the placeholder fires, naming the source field via an `apply_with_field(field: &str, input: &str) -> String` variant.
  - Observable completion: a unit test asserts `apply("\x1b[31m<b>hi</b>")` returns `"&lt;b&gt;hi&lt;/b&gt;"`, that a malformed input returns `"[redacted-invalid]"`, and that the `apply_with_field` variant emits a tracing event with the field name (captured via `tracing-test`).
  - _Requirements: 2.4, 3.4, 6.1, 6.2, 6.3, 6.5_
  - _Boundary: src/server/escape.rs_

- [ ] 2.2 Implement `config.rs`: parse and validate the `server.*` block
  - Create `src/server/config.rs` with `ServerConfig`, `ServerConfigError`, and `pub fn from_policy(policy: &WorkflowPolicy) -> Result<Option<ServerConfig>, ServerConfigError>`.
  - Validate `port` (1..=65535), `bind` (parse as `IpAddr`, default `127.0.0.1`), `min_refresh_interval_seconds` (>=1, default 5), `max_event_log_per_issue` (>=1, default 50).
  - Add `pub fn is_loopback(&self) -> bool` that returns true when `bind` falls within `127.0.0.0/8` or `::1/128`.
  - Register the additive sub-schema for `extension.server.*` in `src/workflow/schema.rs` so the loader does not reject the new keys.
  - Observable completion: unit tests show that a missing `server` block parses to `Ok(None)`, a valid block parses to `Ok(Some(ServerConfig { ... }))` with documented defaults, an out-of-range `port` returns `Err(InvalidPort)`, and a malformed `bind` returns `Err(InvalidBind)`; the loader integration test in `tests/integration_workflow_loader.rs` accepts a `WORKFLOW.md` containing a `server` block.
  - _Requirements: 1.4, 1.5, 7.1, 7.2, 15.1, 15.2, 15.3, 15.4_
  - _Boundary: src/server/config.rs, src/workflow/schema.rs_

- [ ] 2.3 Implement `event_cache.rs` and the `TransitionSubscriberAdapter`
  - Create `src/server/event_cache.rs` with `EventCache` storing one bounded `VecDeque<RecentEventEntry>` per `(RepoId, IssueId)` keyed by orchestrator key, with capacity from `ServerConfig.max_event_log_per_issue`.
  - Implement `EventCache::push(&self, key: &(RepoId, IssueId), entry: RecentEventEntry)` and `EventCache::recent(&self, key: &(RepoId, IssueId)) -> (Vec<RecentEventEntry>, bool /* truncated */)`.
  - Implement `TransitionSubscriberAdapter` that holds an `Arc<EventCache>` and implements roki-mvp's `TransitionSubscriber`: `on_transition` writes one entry; `veto` always returns `Allow`.
  - On terminal-state transition, schedule a grace-window drop of the cache entry so a final snapshot still surfaces the terminal event.
  - Observable completion: a unit test pushes ten entries against a cache with capacity five and asserts `recent` returns five entries with `truncated == true`; a separate test confirms the subscriber adapter never returns `Deny` from `veto`.
  - _Requirements: 2.2, 3.1, 3.6, 13.1, 13.2_
  - _Boundary: src/server/event_cache.rs_

- [ ] 2.4 Implement `projection.rs`: assemble snapshot and per-issue responses
  - Create `src/server/projection.rs` with `Projection` holding `Arc<dyn OrchestratorRead>` and `Arc<EventCache>`.
  - Implement `pub fn build_snapshot(&self) -> Result<SnapshotResponse, ProjectionError>` and `pub fn build_issue_detail(&self, repo: Option<&str>, issue: &str) -> Result<IssueDetailResponse, ProjectionError>`.
  - Map roki-mvp `WorkerState` variants to documented `ProjectionState` strings; map unknown variants to `"unknown"` and surface the original variant name in the response `details` field where applicable.
  - Apply `escape::apply_with_field` to every agent-derived and Linear-derived string field (issue title, description, last-event message, last-error string, label values, tool-result preview).
  - Return `ProjectionError::Unhealthy` (mapped later to 503) and `ProjectionError::AmbiguousRepo` (mapped to 400) and `ProjectionError::NotFound` (mapped to 404).
  - Observable completion: a unit test using a stub `OrchestratorRead` and stub `EventCache` produces a `SnapshotResponse` whose every string field is HTML-escaped and ANSI-stripped (verified by injecting `\x1b[31m<b>` test fixtures); ambiguous-repo and not-found cases produce the correct error variant.
  - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.6, 3.1, 3.2, 3.3, 3.4, 5.1, 5.2, 13.2, 13.3, 13.4_
  - _Boundary: src/server/projection.rs_
  - _Depends: 2.1, 2.3_

- [ ] 2.5 Implement `refresh.rs`: refresh debounce and tracker nudge
  - Create `src/server/refresh.rs` with `RefreshCoordinator` holding `Arc<dyn TrackerRefresh>`, a `Mutex<RefreshDebounceState>`, and the configured `min_refresh_interval`.
  - Implement `pub async fn request(&self, client: SocketAddr) -> RefreshAccepted` that returns `coalesced: true` when a previous request lies within the minimum interval, otherwise calls `tracker.nudge()` and returns the tracker-reported `earliest_fire_at`.
  - Log every request at info level with the client address and the coalescing decision.
  - Observable completion: a unit test against a fake `TrackerRefresh` issues three back-to-back calls with a one-second minimum interval and asserts the second and third return `coalesced: true` while the first returns `coalesced: false`; the test also asserts `tracker.nudge()` was called exactly once.
  - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_
  - _Boundary: src/server/refresh.rs_

- [ ] 2.6 Implement `logging.rs`: per-request tracing layer and request counter
  - Create `src/server/logging.rs` with a tower middleware that emits a structured `tracing::info!` event per request with `method`, `path`, `status`, `duration_ms`, `client_addr`, `correlation_id`, never including the response body.
  - Maintain an `Arc<DashMap<&'static str, AtomicU64>>` (or equivalent) of per-endpoint request counts that the projection can read into `SnapshotResponse.server.request_counters`.
  - Reuse the daemon's existing redaction layer (no new redaction logic).
  - Observable completion: a unit test issues three requests against an in-memory router, captures tracing events with `tracing-test`, and asserts each event contains the documented fields and that `request_counters` reflects three increments on the matching endpoint.
  - _Requirements: 14.1, 14.2, 14.3, 14.5_
  - _Boundary: src/server/logging.rs_

- [ ] 2.7 Implement `router.rs`: axum routes for the three endpoints
  - Create `src/server/router.rs` exposing `pub fn build(state: Arc<AppState>) -> axum::Router`.
  - Wire `GET /api/v1/state` to `Projection::build_snapshot`, `GET /api/v1/<issue>` (with optional `repo` query parameter) to `Projection::build_issue_detail`, `POST /api/v1/refresh` to `RefreshCoordinator::request`.
  - Map `ProjectionError::Unhealthy -> 503`, `AmbiguousRepo -> 400`, `NotFound -> 404`, and any uncaught error to 500 `INTERNAL` with redaction.
  - Set `Content-Type: application/json; charset=utf-8` and `Cache-Control: no-store` on every JSON response via tower middleware.
  - Attach the logging middleware from task 2.6.
  - Observable completion: an integration test in `tests/integration_server_router.rs` runs the router against a stub `OrchestratorRead` and asserts each endpoint returns the documented status, the documented headers, and a body that round-trips through `serde_json` to `SnapshotResponse` / `IssueDetailResponse` / `RefreshAccepted`.
  - _Requirements: 2.1, 2.5, 2.6, 3.1, 3.2, 3.3, 3.5, 3.6, 4.1, 4.2_
  - _Boundary: src/server/router.rs_
  - _Depends: 2.4, 2.5, 2.6_

- [ ] 2.8 Implement `mod.rs`: optional `ServerModule` lifecycle and bind
  - Create `src/server/mod.rs` with `ServerModule` holding `ServerConfig`, `Arc<dyn OrchestratorRead>`, `Arc<dyn TrackerRefresh>`, `Arc<EventCache>`.
  - Implement `pub async fn run(self, shutdown: ShutdownSignal) -> Result<(), ServerError>` that registers `TransitionSubscriberAdapter` with the orchestrator before binding the socket, binds via `axum::serve(TcpListener::bind((self.config.bind, self.config.port)))`, and uses `with_graceful_shutdown(shutdown.fut())`.
  - On non-loopback bind, emit a warn-level tracing event before serving begins; on bind failure, emit an error-level event and return `Ok(())` (no retry, daemon continues without the API).
  - Add a top-level helper `pub fn maybe_spawn(...) -> Option<JoinHandle<()>>` that returns `None` when `ServerConfig::from_policy` returned `Ok(None)`, logs an info-level "API disabled" event in that case, and otherwise spawns the server.
  - Observable completion: an integration test starts the daemon with a `WORKFLOW.md` containing a valid `server.port`, hits `GET /api/v1/state`, observes a 200 response, then sends shutdown and observes a clean exit; a second test starts the daemon with no `server` block and confirms no port is bound and the "API disabled" log fires once.
  - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 7.1, 7.2, 13.1, 13.5, 14.4_
  - _Boundary: src/server/mod.rs_
  - _Depends: 2.2, 2.3, 2.7_

- [ ] 2.9 Wire the server module into `src/main.rs` daemon startup
  - In `src/main.rs`, after `WorkflowLoader` reports a valid policy and after the orchestrator and tracker are constructed, call `server::maybe_spawn(&policy, orchestrator.clone(), tracker.clone(), shutdown.clone())`.
  - Ensure the daemon continues running normally when `maybe_spawn` returns `None` and that shutdown propagates to the spawned task when `Some`.
  - Observable completion: an integration test starts the daemon end-to-end with both API-enabled and API-disabled `WORKFLOW.md` fixtures and asserts the orchestrator runs in both cases; the API-enabled case also passes a TUI-style health-check (`GET /api/v1/state` returns 200).
  - _Requirements: 1.1, 1.2, 13.5_
  - _Boundary: src/main.rs_
  - _Depends: 2.8_

- [ ] 3. TUI binary: rendering, input, and refresh loop

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
  - Implement `async fn state(&self) -> Result<SnapshotResponse, TuiApiError>`, `async fn refresh(&self) -> Result<RefreshAccepted, TuiApiError>`, and `async fn issue(&self, repo: &str, issue: &str) -> Result<IssueDetailResponse, TuiApiError>`.
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
  - Implement a refresh debounce that ignores `Refresh` actions issued within the configured `min_refresh_interval`.
  - Observable completion: a unit test feeds a sequence of synthetic key events and asserts the documented `InputAction` mapping; a separate test asserts the debounce drops the second `Refresh` action issued 100 ms after the first when the minimum interval is 1000 ms.
  - _Requirements: 8.5, 9.1, 10.1, 10.4_
  - _Boundary: crates/roki-tui/src/input.rs_

- [ ] 3.6 Implement `app.rs`: app loop, refresh task, channels, `AppState`
  - Create `crates/roki-tui/src/app.rs` with `AppState` holding the latest `SnapshotResponse`, `last_error: Option<String>`, `acked_escalations: HashSet<String>`, `refresh_in_flight: bool`, `last_refresh_at: Option<Instant>`.
  - Drive the app loop: spawn a refresh task that calls `TuiApiClient::state` at the configured cadence, drain crossterm events, merge updates via `tokio::sync::mpsc` channels, render each tick with `TuiRender::frame`.
  - Clear acknowledgement entries when their underlying escalation is no longer present in the snapshot.
  - On `InputAction::Quit`, restore terminal mode (leave raw mode, show cursor) and exit zero.
  - On `InputAction::Refresh`, call `TuiApiClient::refresh` non-blocking and surface the result in the status bar.
  - Run every incoming string through `sanitize::safe` before storing it in `AppState` (defense in depth even though the server already escapes).
  - Observable completion: an integration test in `crates/roki-tui/tests/app_loop.rs` runs the app loop against a stub HTTP server and asserts: an initial frame renders the workers table, a `Refresh` key triggers exactly one POST, a non-2xx response surfaces in the status bar, and a `Quit` key restores terminal mode and exits.
  - _Requirements: 8.2, 8.3, 8.4, 8.5, 9.1, 9.2, 9.3, 9.4, 10.1, 10.2, 10.3, 10.4_
  - _Boundary: crates/roki-tui/src/app.rs_
  - _Depends: 3.2, 3.3, 3.4, 3.5_

- [ ] 3.7 Implement `main.rs`: CLI, runtime bootstrap, terminal mode setup, Windows guard
  - Create `crates/roki-tui/src/main.rs` with a clap CLI accepting `--url <URL>` (required), `--refresh-interval-ms <MS>` (optional, default 1000), `--quit-key <KEY>` (optional, default `q`).
  - Bootstrap a tokio runtime, call `terminal_caps::detect`, exit non-zero with a not-supported message on Windows, otherwise enter raw mode, hide the cursor, run `app::run(...)`, and on any exit restore the terminal cleanly.
  - Emit one structured tracing startup event (to stderr only, not the daemon log) naming the API URL and the refresh cadence.
  - Observable completion: `cargo run -p roki-tui -- --help` prints the documented flags; a smoke test runs the binary against a stub server, verifies the startup log line is present on stderr, and confirms the binary exits zero on `Quit`.
  - _Requirements: 8.1, 8.2, 8.5, 11.5, 14.4_
  - _Boundary: crates/roki-tui/src/main.rs_
  - _Depends: 3.6_

- [ ] 4. Integration and end-to-end tests across the daemon plus TUI

- [ ] 4.1 Integration test: `GET /api/v1/state` end-to-end with stub orchestrator
  - Add `tests/integration_server_state.rs` that spawns a daemon-shaped harness with a stub `OrchestratorRead`, a stub `EventCache` pre-populated with three issues including one with `\x1b[31m<b>boom</b>` in its last-event message, and a stub `TrackerRefresh`.
  - Assert: 200 status, `Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`, the response body deserializes back into `SnapshotResponse` cleanly, and the offending agent-derived field is `&lt;b&gt;boom&lt;/b&gt;` (escaped, stripped).
  - Observable completion: `cargo test --test integration_server_state` passes.
  - _Requirements: 2.1, 2.4, 2.5, 5.1, 6.1, 6.2, 6.3_
  - _Boundary: tests/integration_server_state.rs_
  - _Depends: 2.7, 2.8_

- [ ] 4.2 Integration test: `GET /api/v1/<issue>` happy, ambiguous, and not-found paths
  - Add `tests/integration_server_issue.rs` that exercises the per-issue endpoint with three fixtures: an existing single-repo issue (200), an ambiguous multi-repo issue without `repo` query (400 `AMBIGUOUS_REPO`), an unknown issue (404 `NOT_FOUND`).
  - Assert each response body matches the `ApiError` envelope shape and the HTTP status codes.
  - Observable completion: `cargo test --test integration_server_issue` passes.
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5_
  - _Boundary: tests/integration_server_issue.rs_
  - _Depends: 2.7, 2.8_

- [ ] 4.3 Integration test: `POST /api/v1/refresh` debounce against fake tracker
  - Add `tests/integration_server_refresh.rs` that issues three back-to-back POSTs against a fake `TrackerRefresh` whose nudge succeeds and then a fourth after the minimum interval has elapsed.
  - Assert: the first and fourth POSTs return `coalesced: false`, the second and third return `coalesced: true`, and the fake tracker's `nudge` counter is exactly 2.
  - Observable completion: `cargo test --test integration_server_refresh` passes.
  - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_
  - _Boundary: tests/integration_server_refresh.rs_
  - _Depends: 2.7, 2.8_

- [ ] 4.4 Integration test: WORKFLOW.md-gated server start and loopback warning
  - Add `tests/integration_server_gating.rs` exercising three startup paths: (a) no `server` block, observe API-disabled log and no listening port; (b) `server.port: <free port>` with no `bind`, observe the loopback bind, info-level "API listening on 127.0.0.1:<port>" log, and a successful `GET /api/v1/state`; (c) `server.bind: 0.0.0.0`, observe a warn-level non-loopback log fires once at startup.
  - Observable completion: `cargo test --test integration_server_gating` passes; `tracing-test` captures show the documented info and warn events.
  - _Requirements: 1.1, 1.2, 1.4, 1.5, 7.1, 7.2_
  - _Boundary: tests/integration_server_gating.rs_
  - _Depends: 2.8, 2.9_

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
  - Add a smoke test target `tests/integration_observability_no_regression.rs` that runs the daemon end-to-end with one API-enabled `WORKFLOW.md` fixture and one API-disabled fixture, exercising the same orchestrator-level scenarios that already pass in roki-mvp's test suite (workspace creation, transition events, recovery), and asserts they still pass in both configurations.
  - Observable completion: `cargo test --test integration_observability_no_regression` passes; the test confirms that enabling the API does not change orchestrator behavior or break any existing mvp-level invariant.
  - _Requirements: 13.1, 13.5_
  - _Boundary: tests/integration_observability_no_regression.rs_
  - _Depends: 2.9_
