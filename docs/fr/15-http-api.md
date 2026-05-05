---
refs:
  id: fr:15-http-api
  kind: fr
  title: "HTTP API"
  spec: roki-observability
  implements:
    - req:roki-observability:1
    - req:roki-observability:2
    - req:roki-observability:3
    - req:roki-observability:4
    - req:roki-observability:5
    - req:roki-observability:6
    - req:roki-observability:7
    - req:roki-observability:12
    - req:roki-observability:13
    - req:roki-observability:14
    - req:roki-observability:15
---

# FR 15: HTTP API

> An optional axum HTTP server. Three endpoints: `GET /api/v1/state` / `GET /api/v1/<issue>` / `POST /api/v1/refresh`. Default off, loopback only, versioned JSON schema centralized in a single shared crate, with HTML escape + ANSI strip on agent / Linear-derived strings. The orchestrator read-only projection that does not duplicate the in-memory state owned by `roki-mvp`.

## Purpose

Without it, an operator's only view of daemon state is `tail | grep` on the tracing log. The HTTP API fills that gap with four guarantees: (a) default off — network exposure is intentional; (b) loopback default to prevent accidental exposure; (c) read-only projection so the orchestrator's source of truth cannot diverge; (d) sanitization so terminal escapes / markup in agent strings cannot damage downstream consumers.

## User-visible Behavior

### Server gating and bind

- **`extension.server.port` not set** ([02-configuration](02-configuration.md)) → the HTTP server does not start, no port is opened, and an `API disabled` info log is emitted.
- **Port set** → at startup, start and bind the server, and log the bind address / port at info severity before reporting ready.
- **Bind failure** (port in use, etc.) → log the offending port + underlying error as a structured error log; the orchestrator continues without the HTTP server (v1 does not retry binding).
- **`extension.server.bind` not set** → bind to `127.0.0.1` (loopback).
- **Non-loopback bind** → emit a warn log noting the bind host and the absence of authentication, and continue.
- **Hot reload**: changes to `extension.server.*` apply on the next daemon restart (v1 does not perform runtime re-bind; the change is only logged).
- **Configuration failure** (type / range validation): refuse to start the server + log the offending key as a structured error log + the orchestrator continues without the API.

### Endpoints

#### `GET /api/v1/state`

- **Response**: HTTP 200 + a JSON body. A daemon snapshot (version, uptime, configured repositories, set of active workers, escalation queue, aggregate token usage, aggregate rate-limit window) and per-issue entries (issue identifier key, current `WorkerState`, latest `Inactive(reason=...)` discriminator when applicable, summary of the latest lifecycle event, latest timestamp, current correlation identifier).
- **Bounded-drift snapshot**: assembled in a fixed three-step order (1) `OrchestratorRead::snapshot()` → (2) per-issue event cache → (3) `OrchestratorRead::escalation_queue()`; entries from the three sub-reads may reflect adjacent moments rather than one instant; the bound between the earliest and latest sub-read is the snapshot drift bound (≤50ms on a developer-class machine, documented in `SPEC.md`).
- **Escalation source**: `escalations` are projected from `OrchestratorRead::escalation_queue()` (the queue is owned by roki-mvp's orchestrator and populated alongside `daemon_directive` events per [14-operator-notifications](14-operator-notifications.md)); the server module does NOT maintain a parallel escalation queue. The per-issue event cache feeds the per-issue detail endpoint only and never derives `escalations`.
- **Headers**: `Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`.
- **Unhealthy state**: HTTP 503 + a JSON error body (the names of unhealthy subsystems).
- **API self-counter**: the daemon's internal API request counter is exposed in this endpoint's `server` block.

#### `GET /api/v1/<issue>`

- **Response**: HTTP 200 + a JSON body. Per-issue detail (issue identifier key, current state, latest `Inactive(reason=...)` discriminator when applicable, recent lifecycle event log within the documented retention window, latest error, configured permission strategy, workspace path).
- **Not found**: HTTP 404 + a JSON error body.
- **Truncation**: a recent event log that exceeds the documented max is truncated and a `truncated: true` field is added.

#### `POST /api/v1/refresh`

- **Response**: HTTP 202 + a JSON body (indicates the request was accepted; whether it was coalesced; if 429 backoff is in effect, an estimate of the earliest fire time).
- **Mutation scope**: only rescheduling tracker refresh (via the `TrackerRefresh` from [12-extension-surface](12-extension-surface.md)). Worker cancel / retry / reschedule / terminate are **not** performed.
- **During 429 backoff**: the request is accepted + queued + fires at the end of the backoff.
- **Coalescing**: applies the documented minimum refresh interval per scope. Bursts are aggregated into a single fire.
- **Logging**: each request is logged at info severity (client address, coalescing decision).

### Sanitization (common to all endpoints)

- **HTML escape**: every string field originating from the agent (last message / last error / tool-result preview) and from Linear (issue title / description / label) is escaped before serialization.
- **ANSI strip**: terminal escape sequences are stripped from agent / Linear-derived strings.
- **Defense in depth on the TUI side**: `roki-tui` also strips ANSI / control characters from received strings ([16-roki-tui](16-roki-tui.md)).
- **Sanitize failure** (invalid UTF-8, etc.) → replace the string with a sanitized placeholder marker and log the offending field name.

### Schema stability and versioning

- **Single source of truth**: every request / response shape is declared in the `roki-api-types` crate. The server module and `roki-tui` both import these types; neither side may redefine them locally.
- **`api_version` field**: every JSON response body carries an `api_version` field whose value matches `roki_api_types::API_VERSION` (currently `"v1"`).
- **Versioning**: currently `/api/v1/`. Within `/api/v1/`, additions are additive only (new optional fields). Breaking changes (renames, removals, type changes) introduce `/api/v2/` without breaking existing `/api/v1/` consumers.
- **roki-specific fields** (e.g. `Inactive.reason` discriminator, escalation queue entry, multi-repo `(repo, issue)` keying as separate `repo` / `issue` fields): documented per-field in `SPEC.md` under `## Observability HTTP API`.

### Logging (no body leakage)

- **Per-request structured log**: method / path / response status / request duration / client address / per-request correlation identifier.
- **Bodies are not emitted**: in v1, request / response bodies are not logged (to prevent agent strings from leaking into logs).
- **Secret redaction**: reuses the same redaction layer as the orchestrator log ([13-observability-logs](13-observability-logs.md)).

### Shared types crate and read-only projection

- **`roki-api-types` crate**: every request / response type used across the API lives in this single crate. Neither the server module nor `roki-tui` may redefine them locally.
- **Do not import internal types**: roki-mvp's in-memory internal types (the actual `WorkerState`, etc.) are not imported; they are declared as separate projection types. The server module maps from the in-memory model.
- **No parallel store**: there is no API-side persistent state; each response is assembled from the live in-memory model at request time.
- **Forward compatibility**: when roki-mvp adds new state / event types, existing endpoints keep working (unknown internal states map to a documented fallback string).
- **Independence**: the orchestrator core works even with the HTTP server disabled (the dependency direction is server → orchestrator only).

## Capabilities

- **Opt-in by config**: default off, loopback only; only what is explicitly enabled is exposed to the network.
- **Read-only mostly**: only `/refresh` mutates, and even that only reschedules a tracker poll.
- **Schema drift impossible**: server and TUI import the same crate, so a breaking change makes both sides fail to compile.
- **Layered sanitization**: stripped on both server and TUI.
- **Self-observable**: the API's own usage count is exposed by the API itself.

## Boundaries

- **Authentication / Authorization** are out of scope for v1 (loopback assumption).
- **TLS** is out of scope for v1.
- **Web UI** is out of scope (a future spec).
- **Adding mutating endpoints** (cancel / retry / pause / resume / workspace operations) is out of scope.
- **Persistent metrics / time-series** are out of scope (live snapshot only).
- **Per-request body capture** is out of scope (an opt-in is conceivable in the future).
- **Runtime re-bind** is out of scope (a restart is required).
- **Windows support** is out of scope.

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-observability`; Boundary Strategy > "Observability (HTTP + TUI)"
- **Requirements**:
  - `roki-observability Req 1` - `Req 7`: server gating / endpoints / loopback / schema stability / sanitization
  - `roki-observability Req 12` - `Req 15`: shared types / projection / logging / configuration
  - `roki-mvp Req 13.1`, `Req 13.3`: `OrchestratorRead` trait, `TrackerRefresh` trait
- **Design**:
  - `.kiro/specs/roki-observability/design.md`
- **Related FR**: 02-configuration, 04-state-machine-and-recovery, 03-linear-integration, 12-extension-surface, 13-observability-logs, 16-roki-tui
