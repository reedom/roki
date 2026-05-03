---
refs:
  id: requirements:roki-observability
  kind: requirements
  title: "roki-observability Requirements"
  spec: roki-observability
  implements:
    - roadmap
---

# Requirements Document

## Project Description (Input)
Operators running roki-mvp today have no programmatic surface to inspect "what is roki currently doing right now?" — they tail tracing logs and grep, which is the opposite of the human-scannable view they actually need. roki-observability adds an optional axum HTTP server module to the roki daemon and a separate `roki-tui` ratatui binary so operators can see active workers, per-issue state, the last lifecycle event, the escalation queue, and token / rate-limit snapshots in seconds rather than minutes. The HTTP server is loopback-only by default, gated by `WORKFLOW.md` `server.port` (off by default), and exposes a symphony-compatible JSON API: `GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh`. The TUI is a separate process that reads the JSON API on a refresh loop and supports escalation acknowledgement plus a manual refresh nudge. Read-only projection in v1: no cancel, retry, or reschedule mutating endpoints; `POST /refresh` only nudges the poller. All agent-derived strings (issue titles, descriptions, last messages, error strings) are HTML-escaped and ANSI-stripped before rendering, so a malicious or unlucky agent payload cannot inject markup or terminal control sequences. Shared `serde` types live in a single crate consumed by both the daemon's server module and the `roki-tui` binary so the API schema cannot drift between sides. Out of scope: authn / auth tokens, web UI, mutating control plane beyond `/refresh`, persistent metrics or time-series storage. Reuse roki-mvp's in-memory state model — do not duplicate state structs.

## Introduction

The roki-observability specification adds the operator-facing observability surface to roki: an optional HTTP API that exposes the daemon's in-memory orchestrator state as JSON over loopback, and a separate ratatui-based TUI binary (`roki-tui`) that consumes that API. The HTTP module is a read-only projection of roki-mvp's in-memory state — it subscribes to the orchestrator's transition event bus and serves snapshots; the orchestrator itself does not depend on the API and the daemon continues to function with the API disabled. The JSON schema mirrors symphony's API (`GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh`) so future dashboard alternates (web UI, dashboards, scripts) interoperate without per-tool variants.

This spec extends roki-mvp by plugging into its declared extension seams: the transition event bus (Requirement 8.2 and 8.3 of roki-mvp) and the `WORKFLOW.md` schema (Requirement 6.5 of roki-mvp). It does not modify the orchestrator state machine and does not add mutating endpoints beyond a refresh nudge that asks the tracker poller to reschedule its next tick.

## Boundary Context

- **In scope**: an optional axum HTTP server module compiled into the roki daemon binary and gated by a `WORKFLOW.md` `server.port` key (off by default); three JSON HTTP endpoints `GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh`; symphony-compatible JSON schema; loopback-only default bind with operator opt-in to bind on a non-loopback interface; HTML-escaping plus ANSI stripping of every agent-derived string field before serialization; a separate `roki-tui` ratatui binary that consumes the JSON API on a refresh loop and renders the active worker list, per-issue state and last event, escalation queue, token / rate-limit snapshots; TUI escalation acknowledgement (a local UI flag, no daemon-side effect in v1) and a TUI manual refresh action that issues `POST /api/v1/refresh`; macOS plus Linux terminal compatibility with iTerm2 / Ghostty / WezTerm / Alacritty as the primary targets and a graceful informational degradation on macOS Terminal.app for RGB color limits; a single shared crate of `serde` types consumed by both the server module and the TUI client so the schema cannot drift; configuration of refresh cadence and bind address through `WORKFLOW.md` and CLI flags; structured logging of every API request (method, path, status, duration, correlation id) through the existing tracing pipeline.
- **Out of scope**: authentication and authorization (loopback-only, multi-user is out of scope; binding to a non-loopback interface is the operator's risk and is documented but not protected by tokens or TLS in v1); a web UI (the JSON API enables a future web UI but no web UI is built here); any mutating endpoints beyond `/api/v1/refresh` — no cancel, retry, reschedule, pause, resume, or workspace operations; persistent metrics or time-series storage of token usage / rate-limit history (every snapshot is read live from in-memory state at request time); any change to roki-mvp's orchestrator state machine, state set, transition rules, or workspace lifecycle; any duplication of roki-mvp's state structs — the API serializes from the existing in-memory model via projection types only; Windows support; HTTPS / TLS termination (loopback assumption); persistent TUI session state across restarts beyond what the user types in the current session; agent-side roki tools (the API is not exposed back to the agent through the tool registry).
- **Adjacent expectations**: roki-mvp publishes the transition event bus and the in-memory state model the API projects from; roki-mvp's `WORKFLOW.md` schema reserves an extension namespace under which `server.*` keys can live without breaking the loader; roki-mvp's tracking poller exposes a refresh-nudge API the HTTP `POST /api/v1/refresh` endpoint can call; roki-mvp's tracing pipeline is the destination for all server and TUI logs; the operator runs the TUI on the same host as the daemon (loopback) or explicitly opens a non-loopback bind and accepts the documented exposure risk.

## Requirements

### Requirement 1: Optional HTTP Server Module Gated by Configuration

**Objective:** As an operator, I want the HTTP API server to be off by default and to start only when I explicitly enable it through `WORKFLOW.md`, so that the daemon's network exposure stays opt-in and zero-config users have no surprise listening port.

#### Acceptance Criteria
1. When the operator starts the roki daemon and no `server.port` value is configured in `WORKFLOW.md`, the roki daemon shall not start the HTTP server, shall not bind any port, and shall log an info-level event stating that the API is disabled.
2. When the operator configures a valid `server.port` value in `WORKFLOW.md`, the roki daemon shall start the HTTP server during startup, bind the configured port, and log an info-level event that names the bind address and port before reporting ready.
3. If the configured `server.port` is invalid, in use, or otherwise fails to bind, the roki daemon shall log a structured error event that names the offending port and the underlying bind error, shall continue running the orchestrator without the HTTP server, and shall not retry the bind in v1.
4. When the operator does not configure a `server.bind` host in `WORKFLOW.md`, the roki daemon shall bind the HTTP server to the loopback address (127.0.0.1) only.
5. When the operator configures a non-loopback `server.bind` host, the roki daemon shall bind that host, shall log a warn-level event that names the bind host and explains the absence of authentication, and shall continue startup.
6. When `WORKFLOW.md` is hot-reloaded and the `server.*` block changes, the roki daemon shall log the change and shall apply the new configuration on the next daemon restart, and shall not reconfigure the listening socket at runtime in v1.

### Requirement 2: Read-Only State Projection Endpoint

**Objective:** As an API consumer (TUI, future web UI, or script), I want a single endpoint that returns the full current orchestrator state as JSON, so that I can render an up-to-date view without polling per-issue endpoints in a loop.

#### Acceptance Criteria
1. When a client sends `GET /api/v1/state`, the roki daemon shall return HTTP 200 with a JSON body containing the daemon snapshot (version, uptime, configured repositories, currently active workers, escalation queue, aggregate token usage, aggregate rate-limit window).
2. The roki daemon shall serialize each per-issue entry in the snapshot with the `(repo, issue)` key, current `WorkerState`, the most recent lifecycle event summary, the timestamp of the last lifecycle event, and the current correlation identifier.
3. The roki daemon shall produce the snapshot from a single coherent read of the in-memory state model so that no per-issue entry refers to a state older than the snapshot's own timestamp by more than the documented snapshot drift bound.
4. The roki daemon shall HTML-escape and ANSI-strip every string field in the snapshot that originates from an agent or from Linear (issue titles, descriptions, last-message content, error strings) before serialization.
5. The roki daemon shall set the JSON `Content-Type` response header to `application/json; charset=utf-8` and shall include a `Cache-Control: no-store` header so intermediaries do not retain snapshots.
6. If the orchestrator is unhealthy or partially initialized at the moment of request, the roki daemon shall return HTTP 503 with a JSON error body that names the unhealthy subsystem.

### Requirement 3: Per-Issue Detail Endpoint

**Objective:** As an API consumer, I want a per-issue endpoint that returns deeper detail for a specific `(repo, issue)` than the global snapshot includes, so that I can drill into one ticket without scanning the whole state response.

#### Acceptance Criteria
1. When a client sends `GET /api/v1/<issue>`, the roki daemon shall return HTTP 200 with a JSON body containing the per-issue detail (key, current state, recent lifecycle event log up to the documented retention window, last error if any, configured permission strategy, workspace path).
2. The roki daemon shall accept the `<issue>` path segment as the issue identifier; if the same issue identifier is configured to route to multiple repositories, the roki daemon shall require a `repo` query parameter to disambiguate and shall return HTTP 400 with a structured error if it is missing.
3. If the requested issue is not present in the in-memory state, the roki daemon shall return HTTP 404 with a JSON error body that names the issue identifier and any disambiguation parameter values used.
4. The roki daemon shall HTML-escape and ANSI-strip every agent-derived or Linear-derived string field in the per-issue detail response.
5. The roki daemon shall include the same `Content-Type: application/json; charset=utf-8` and `Cache-Control: no-store` headers as the snapshot endpoint.
6. The roki daemon shall enforce a documented maximum recent-event log length per issue and shall paginate or truncate beyond it; when truncated, the response shall include a `truncated: true` field.

### Requirement 4: Refresh Nudge Endpoint

**Objective:** As an API consumer, I want a single mutating endpoint that nudges the daemon to refresh tracker state immediately rather than waiting for the next polling tick, so that an operator can observe a change in Linear without waiting up to five minutes.

#### Acceptance Criteria
1. When a client sends `POST /api/v1/refresh`, the roki daemon shall request the tracker adapter to schedule a refresh tick at the next available opportunity and shall return HTTP 202 with a JSON body indicating the request was accepted.
2. The roki daemon shall not perform any state-mutating action beyond requesting a tracker refresh; the endpoint shall not cancel, retry, reschedule, or terminate any worker.
3. If the tracker adapter is in a 429 backoff window or otherwise unable to immediately accept a refresh, the roki daemon shall still accept the request, shall queue it to fire when the backoff window elapses, and shall include in the response body the estimated earliest fire time.
4. The roki daemon shall enforce a documented minimum refresh interval per scope and shall coalesce a burst of refresh calls within that interval into a single tracker refresh; the response shall report whether the request was coalesced.
5. The roki daemon shall log every refresh request at info level with the requesting client address and any coalescing decision.

### Requirement 5: Symphony-Compatible JSON Schema

**Objective:** As a future web-UI author or external dashboard implementer, I want the JSON schema served by roki to match symphony's `/api/v1/state` shape so that I can target both tools with a single client.

#### Acceptance Criteria
1. The roki daemon shall serialize the snapshot returned by `GET /api/v1/state` using field names and structural shapes compatible with symphony's documented `/api/v1/state` contract.
2. The roki daemon shall serialize the per-issue detail returned by `GET /api/v1/<issue>` using field names and structural shapes compatible with symphony's documented per-issue endpoint.
3. The roki daemon shall serialize the response to `POST /api/v1/refresh` using field names and structural shapes compatible with symphony's documented refresh-nudge endpoint.
4. Where roki has additional fields not present in symphony (for example multi-repo `(repo, issue)` keying), the roki daemon shall add those fields under names that do not collide with symphony's fields and shall document the additions in `SPEC.md`.
5. The roki daemon shall version the JSON contract under the `/api/v1/` prefix so that future breaking changes can introduce a `/api/v2/` namespace without disturbing existing consumers.

### Requirement 6: Untrusted-String Escaping and ANSI Stripping

**Objective:** As an operator, I want every agent-derived and Linear-derived string to be HTML-escaped and ANSI-stripped before it enters the JSON response or the TUI render, so that a malicious or unlucky agent payload cannot inject markup or terminal control sequences into a downstream consumer.

#### Acceptance Criteria
1. The roki daemon shall HTML-escape every string field in the JSON API that carries agent-derived content (last message, last error, tool-result preview) so that downstream HTML renderers cannot interpret embedded markup.
2. The roki daemon shall HTML-escape every Linear-derived string field (issue title, description, label values) before serialization.
3. The roki daemon shall ANSI-strip every string field carrying agent-derived or Linear-derived content so that no terminal escape sequence reaches a TUI renderer or a log consumer through the API.
4. The roki-tui binary shall ANSI-strip and remove control-character payloads from every string it receives before rendering, so that defense in depth applies even if a future API change forgets to strip.
5. If a string fails escaping or stripping (for example invalid UTF-8 sequences), the roki daemon shall replace the string with a sanitized placeholder marker and shall log the event with the offending field name.

### Requirement 7: Loopback-Only Default and Documented Exposure Risk

**Objective:** As an operator, I want the HTTP API to default to loopback-only binding and to surface a clear warning when I configure a non-loopback bind, so that I cannot accidentally expose the unauthenticated read-only API to a network without intent.

#### Acceptance Criteria
1. The roki daemon shall default the HTTP server bind host to `127.0.0.1` when `server.bind` is unset.
2. When the operator sets `server.bind` to a non-loopback address (any interface that resolves outside `127.0.0.0/8` and `::1/128`), the roki daemon shall emit a warn-level log event at startup that names the bind host, states the absence of authentication, and points the operator at the `SPEC.md` security note.
3. The roki daemon shall document the exposure risk and the absence of authentication in `SPEC.md` and in the `WORKFLOW.example.md` file under the `server.*` section.
4. The roki daemon shall not implement authentication or authorization on the HTTP API in v1; any future authn / authz is explicitly out of scope and is the responsibility of a later spec.

### Requirement 8: Separate `roki-tui` Ratatui Binary

**Objective:** As an operator, I want a separate ratatui binary `roki-tui` that I can run on demand against the daemon's API, so that the TUI is independent of the daemon process lifecycle and can be opened and closed without affecting the daemon.

#### Acceptance Criteria
1. The roki repository shall ship a `roki-tui` binary as a separate Cargo target in the same workspace as the daemon.
2. When the operator invokes `roki-tui` with a target API URL, the roki-tui binary shall connect to that URL, fetch the initial snapshot via `GET /api/v1/state`, and render the live state view within the documented startup window.
3. While running, the roki-tui binary shall refresh its view by repeatedly calling `GET /api/v1/state` at a configurable cadence; when the API returns a non-2xx status, the roki-tui binary shall display the error in the status bar without exiting.
4. The roki-tui binary shall display, at minimum, the active worker list, per-issue current state, the last lifecycle event summary per issue, the escalation queue, the aggregate token usage, and the aggregate rate-limit window.
5. The roki-tui binary shall exit cleanly when the operator presses the documented quit key, restoring the terminal to its prior mode.
6. The roki-tui binary shall consume the same shared `serde` types as the server so that no duplicate parsing logic exists across the two sides.

### Requirement 9: TUI Escalation Acknowledgement

**Objective:** As an operator triaging escalations, I want the TUI to let me acknowledge an escalation row so that I can mark it as "I have seen this" without leaving the TUI and without affecting the daemon's state.

#### Acceptance Criteria
1. When the operator selects an escalation row in the TUI and presses the documented acknowledge key, the roki-tui binary shall mark that row as acknowledged in its local in-memory UI state.
2. The roki-tui binary shall visually distinguish acknowledged from unacknowledged escalations using both color and a non-color glyph so that the distinction survives Terminal.app and other RGB-color-limited terminals.
3. The roki-tui binary shall clear acknowledgement state when the underlying escalation is no longer present in the API snapshot.
4. The roki-tui binary shall not send any acknowledgement signal to the daemon in v1; acknowledgement is a local UI affordance only, and a future spec may extend the API to persist it.

### Requirement 10: TUI Refresh Action

**Objective:** As an operator, I want a key in the TUI that triggers `POST /api/v1/refresh` so that I can ask the daemon to re-poll Linear right now without waiting for the next cadence.

#### Acceptance Criteria
1. When the operator presses the documented refresh key, the roki-tui binary shall issue `POST /api/v1/refresh` to the configured API URL and shall display the response status in the status bar.
2. While the refresh request is in flight, the roki-tui binary shall show a non-blocking visual indicator and shall continue to render incoming snapshot updates.
3. If the refresh request fails or returns a non-2xx status, the roki-tui binary shall display the error in the status bar with the HTTP status and a short error message and shall not exit.
4. The roki-tui binary shall respect the API's documented minimum refresh interval and shall debounce repeated key presses within that interval into a single in-flight request.

### Requirement 11: Terminal Compatibility and Graceful Degradation

**Objective:** As an operator on macOS or Linux, I want the TUI to work on iTerm2, Ghostty, WezTerm, and Alacritty as primary targets and to degrade gracefully on macOS Terminal.app, so that my terminal choice does not prevent me from using roki-tui.

#### Acceptance Criteria
1. The roki-tui binary shall render its primary view correctly on iTerm2, Ghostty, WezTerm, and Alacritty on macOS and Linux.
2. When the roki-tui binary detects it is running on a terminal that does not support 24-bit RGB color (for example macOS Terminal.app), it shall fall back to a 16-color or 256-color palette and shall display a one-time informational notice in the status bar at startup.
3. The roki-tui binary shall not depend on terminal features unavailable on the listed primary terminals (no Sixel, no Kitty graphics protocol, no advanced mouse-tracking dependence).
4. The roki-tui binary shall use only printable ASCII or commonly supported Unicode glyphs for state markers so that fallback fonts do not produce missing-glyph squares on Terminal.app.
5. The roki-tui binary shall not target Windows in v1; if invoked on Windows it shall print a not-supported message and exit non-zero.

### Requirement 12: Shared API Types Crate

**Objective:** As a maintainer of both the daemon and the TUI, I want the JSON request and response shapes to live in a single shared crate so that schema drift between server and client is structurally impossible.

#### Acceptance Criteria
1. The roki repository shall include a single shared crate (for example `roki-api-types`) that defines every `serde` type that crosses the API boundary in either direction.
2. The roki daemon's HTTP server module shall depend on the shared crate for all request and response types and shall not redefine any of them locally.
3. The roki-tui binary shall depend on the shared crate for all request and response types and shall not redefine any of them locally.
4. The shared crate shall be the only place where `(repo, issue)` keying, `WorkerState` projection, lifecycle event summaries, escalation entries, and token / rate-limit snapshot shapes are declared for the API.
5. The shared crate shall not import roki-mvp's internal in-memory state types directly; instead it shall declare projection types that the daemon's server module maps from the in-memory model so that internal refactors of the orchestrator do not break the API.

### Requirement 13: Read-Only Projection — No Duplicated State

**Objective:** As a roki-mvp maintainer, I want the observability spec to project from the existing in-memory orchestrator state without duplicating its state structs, so that there is exactly one source of truth for orchestrator state and the API stays consistent on every refactor.

#### Acceptance Criteria
1. The roki daemon shall implement the HTTP server module as a read-only subscriber to the orchestrator's transition event bus and an on-demand reader of the in-memory state model.
2. The roki daemon shall not maintain a parallel persistent state store for the API; every API response shall be assembled from the live in-memory model at request time.
3. The roki daemon shall not duplicate roki-mvp's `WorkerState`, `TransitionEvent`, or `NormalizedIssue` types inside the API server module; instead the server shall import them from the orchestrator and project them into the shared API types.
4. When roki-mvp adds a new state or event type, the roki daemon shall continue to serve existing endpoints; unknown internal states shall map to a documented fallback string in the projection.
5. The roki daemon shall continue to function correctly with the HTTP server disabled; the orchestrator core shall not depend on the API in any direction.

### Requirement 14: HTTP Server Observability and Logging

**Objective:** As an operator, I want every HTTP request and every TUI session start to produce a structured log event through the existing tracing pipeline, so that I can debug API consumers from the same logs I already use for the daemon.

#### Acceptance Criteria
1. When the roki daemon serves an HTTP request, it shall emit a structured tracing event that includes the method, the path, the response status, the request duration, the client address, and a per-request correlation identifier.
2. The roki daemon shall not log request or response bodies in v1; it shall log only the documented metadata fields so that agent-derived strings do not leak into logs through the API path.
3. The roki daemon shall apply the same secret-redaction layer to API logs as it does to orchestrator logs.
4. When the roki-tui binary starts, it shall log a structured event to its own stderr (not the daemon log) that names the API URL it connects to and the refresh cadence in use.
5. The roki daemon shall expose an internal counter of API requests per endpoint visible via `GET /api/v1/state` under the documented `server` block of the snapshot, so that operators can see whether the API is being used.

### Requirement 15: Configuration via WORKFLOW.md `server.*` Block

**Objective:** As an operator, I want all observability configuration to live under a single `server.*` block in `WORKFLOW.md` so that I can opt in, change ports, and tune cadence without editing the daemon binary or environment variables.

#### Acceptance Criteria
1. The roki daemon shall read its HTTP server configuration from the `server.*` extension block of the parsed `WorkflowPolicy`.
2. The `server.*` block shall support at minimum `port` (integer, optional), `bind` (string, optional, default `127.0.0.1`), `min_refresh_interval_seconds` (integer, optional with documented default), and `max_event_log_per_issue` (integer, optional with documented default).
3. If any `server.*` value fails type or range validation at startup, the roki daemon shall refuse to start the HTTP server, shall log a structured error that names the offending key, and shall continue running the orchestrator without the API.
4. The roki daemon shall accept absence of the entire `server` block as the explicit "API disabled" state.
5. The roki daemon shall document the `server.*` block, default values, and validation rules in `SPEC.md` and `WORKFLOW.example.md`.
