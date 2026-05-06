---
refs:
  id: fr:10-http-api
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
  related:
    - fr:02-configuration
    - fr:03-linear-admission
    - fr:07-recovery
    - fr:08-observability-logs
    - fr:06-failure-handling
    - fr:11-roki-tui
    - fr:01-engine-model
    - fr:09-log-access-cli
---

# FR 10: HTTP API

> An optional axum HTTP server. Read-only endpoints over the diff cache, cycle history, structured event ring buffer, escalation queue, plus a single mutating endpoint that schedules a Linear refresh nudge. Default off, loopback only, versioned JSON schema centralized in a shared crate, with HTML escape + ANSI strip on agent / Linear-derived strings.

## Purpose

Without it, an operator's only view of daemon state is `tail | grep` on the structured event log. The HTTP API fills that gap with four guarantees: (a) default off — network exposure is intentional; (b) loopback default to prevent accidental exposure; (c) read-only projection so the diff cache cannot diverge; (d) sanitization so terminal escapes / markup in agent strings cannot damage downstream consumers. Endpoint set: tickets + cycles + iters + events + escalations + healthz + refresh.

## User-visible Behavior

### Server gating and bind

The observability HTTP API is **separate from the Linear webhook receiver** ([03-linear-admission §Webhook intake](03-linear-admission.md)). The webhook receiver listens on `[linear.webhook]` (internet-facing); the observability API listens on `[api]` (loopback by default). Sharing one port between the two would force the read-only observability surface onto the public ingress; keeping them separate prevents accidental exposure.

- **`[api].port` not set in `roki.toml`** → the API server does not start, no port is opened, and an `api_disabled` info log is emitted.
- **Port set** → at startup, start and bind the server, and log the bind address / port at info severity before reporting ready.
- **Bind failure** (port in use, etc.) → log the offending port + underlying error as a structured error log; the daemon continues without the HTTP server (the daemon does not retry binding).
- **`[api].bind` not set** → bind to `127.0.0.1` (loopback).
- **Non-loopback bind** → emit a warn log noting the bind host and the absence of authentication, and continue.
- **Hot reload**: changes to `[api].*` apply on the next daemon restart; no runtime re-bind.
- **Configuration failure** (type / range validation): refuse to start the server + log the offending key + the daemon continues without the API.

HTTP server settings live under `roki.toml [api]`.

### Endpoints

#### `GET /api/healthz`

Liveness probe. HTTP 200 with a small JSON body (version, uptime, configured repositories, daemon-internal API request counter). No per-ticket data.

#### `GET /api/tickets`

Cache snapshot of every ticket the daemon currently tracks.

- **Response body** (per entry): ticket identifier, repo (admission-resolved), current status / labels / assignee, in-flight cycle id (or null), last event timestamp.
- **Source**: in-memory diff cache ([07-recovery §Diff cache](07-recovery.md)).
- **Bounded drift**: the snapshot is assembled in a single read pass; there is no cross-source merge.
- **Headers**: `Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`.

#### `GET /api/tickets/{id}`

Per-ticket detail.

- **Response body**: same fields as the list view plus the most recent N events for the ticket (drawn from the event ring buffer; N is bounded; on overflow the response carries `truncated: true`).
- **Not found** → HTTP 404 with a JSON error body.

#### `GET /api/tickets/{id}/cycles`

Cycle history for the ticket.

- **Response body** (per entry): cycle id, kind (`rule` / `cleanup` / `failure`), trigger (`webhook` / `cold_start`), started_at, ended_at, terminal directive or failure kind.
- **Source**: scan of the ticket's session tempdir under `[paths].session_root` (cycle metadata files).

#### `GET /api/tickets/{id}/cycles/{cycle_id}/iters/{n}/{phase}/{stream}`

HTTP wrapper around `roki log` ([09-log-access-cli §`roki log`](09-log-access-cli.md)). `phase` ∈ `pre` / `run` / `post`. `stream` ∈ `stdout` / `stderr` / `response` / `terminal` / `events` / `exit_code`. `n` is an absolute iter number; relative iter (`-1`, etc.) is not supported on the HTTP path because URLs prefer absolute.

#### `GET /api/events`

Structured event stream.

- **Query parameters**: `since=<seq>` for cursor-based range, `kind=<event_kind>`, `ticket=<id>`, `cycle=<uuid>`. Filters compose with AND.
- **Response body**: an ordered list of events from the in-memory ring buffer ([08-observability-logs §Tier 3](08-observability-logs.md)). When `since` is older than the ring's oldest sequence number, the response carries `gap: true` and the operator should consult the file destination for the missing range.

WebSocket / SSE push is deferred. Live tail is achieved by polling `since=<latest_seq>` from the TUI or `roki events --tail`.

#### `GET /api/escalations`

Escalation queue dump ([06-failure-handling §Escalation queue](06-failure-handling.md)). The queue surfaces daemon-stuck failures only — failure-handler cycles that themselves failed and daemon-internal errors with no cycle association. Per entry: ticket id (or null for cycle-less daemon errors), cycle id (or null), kind, phase, timestamp, error text. The list is bounded by ring size.

#### `POST /api/refresh`

Linear refresh nudge.

- **Response**: HTTP 202 + a JSON body indicating whether the request was coalesced and, if 429 backoff is in effect, an estimate of the earliest fire time.
- **Mutation scope**: only rescheduling the tracker poll. No worker cancel / retry / reschedule / terminate.
- **During 429 backoff**: the request is dropped (logged), not queued. (See [03-linear-admission §Refresh nudge](03-linear-admission.md).)
- **Coalescing**: bursts inside the cadence cap are aggregated into a single fire.
- **Logging**: each request is logged at info severity (client address, coalescing decision).

### Sanitization (common to all endpoints)

- **HTML escape**: every string field originating from a phase subprocess (last directive payload field, last error text, escalation entry text) and from Linear (ticket title / description / label) is escaped before serialization.
- **ANSI strip**: terminal escape sequences are stripped from agent / Linear-derived strings.
- **Defense in depth on the TUI side**: `roki-tui` also strips ANSI / control characters from received strings ([11-roki-tui](11-roki-tui.md)).
- **Sanitize failure** (invalid UTF-8, etc.) → replace the string with a sanitized placeholder marker and log the offending field name.

### Schema stability

- **Single source of truth**: every request / response shape is declared in the `roki-api-types` crate. The server module, `roki-tui`, and `roki log` / `roki events` / `roki repo` all import these types; no client-side redefinition.
- **No URL versioning**: paths are `/api/...` directly. Breaking changes are driven by the `roki` binary release the operator runs; clients pinned to a `roki` version automatically pin the matching `roki-api-types` schema.
- **Additive-by-default**: new optional response fields are added freely. Renames, removals, and type changes are landed alongside a `roki` release that bumps the server, the TUI, and the CLI surfaces atomically.
- **roki-specific fields** (cycle id, cycle kind, cycle trigger, failure kind, escalation entry, multi-repo `(repo, ticket)` keying as separate `repo` / `ticket` fields) are documented per-field via Rust doc comments on the corresponding types in the `roki-api-types` crate.

### Logging (no body leakage)

- **Per-request structured log**: method / path / response status / request duration / client address / per-request correlation identifier.
- **Bodies are not emitted**: request / response bodies are not logged (to prevent agent strings from leaking into logs).
- **Secret redaction**: reuses the same redaction layer as the daemon log ([08-observability-logs](08-observability-logs.md)).

### Shared types crate and read-only projection

- **`roki-api-types` crate**: every request / response type used across the API lives in this single crate.
- **Do not import internal types**: roki-mvp's in-memory internal types (the actual diff cache entries, etc.) are not imported by the server module; they are declared as separate projection types. The server module maps from the in-memory model.
- **No parallel store**: there is no API-side persistent state; each response is assembled from the live in-memory model and the on-disk per-ticket captures at request time.
- **Forward compatibility**: when roki-mvp adds new event kinds or cycle kinds, existing endpoints keep working (unknown internal kinds map to a documented fallback string).
- **Independence**: the daemon core works even with the HTTP server disabled (the dependency direction is server → daemon core only).

## Capabilities

- **Opt-in by config**: default off, loopback only; only what is explicitly enabled is exposed to the network.
- **Read-only mostly**: only `/refresh` mutates, and even that only reschedules a tracker poll.
- **Schema drift impossible**: server and clients import the same crate, so a breaking change makes both sides fail to compile.
- **Layered sanitization**: stripped on both server and TUI.
- **Self-observable**: the API's own usage count is exposed in `/healthz`.
- **Backed by the in-memory ring + on-disk capture**: events are answered from the ring; per-iter captures are answered from disk via the same storage abstraction `roki log` uses.

## Boundaries

- **Authentication / Authorization** are out of scope (loopback assumption).
- **TLS** is out of scope.
- **Web UI** is out of scope (a future spec).
- **Adding mutating endpoints** (cancel / retry / pause / resume / workspace operations) is out of scope.
- **Persistent metrics / time-series** are out of scope (live snapshot only).
- **Per-request body capture** is out of scope.
- **Runtime re-bind** is out of scope (a restart is required).
- **WebSocket / SSE push** is out of scope; polling `since` is the supported model.
- **Windows support** is out of scope.
- **URL versioning** is out of scope. Schema and binary advance together.

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-observability`; Boundary Strategy > "Observability (HTTP + TUI)".
- **Requirements**:
  - `roki-observability Req 1` – `Req 7`: server gating / endpoints / loopback / schema stability / sanitization.
  - `roki-observability Req 12` – `Req 15`: shared types / projection / logging / configuration.
  - `roki-mvp Req 13.3`: tracker-refresh contract (refresh nudge consumer).
- **Design**:
  - `.kiro/specs/roki-observability/design.md` (pending rewrite to reflect the simplified endpoint set).
- **Related FR**: [02-configuration](02-configuration.md), [03-linear-admission](03-linear-admission.md), [07-recovery](07-recovery.md), [08-observability-logs](08-observability-logs.md), [06-failure-handling](06-failure-handling.md), [11-roki-tui](11-roki-tui.md), [01-engine-model](01-engine-model.md), [09-log-access-cli](09-log-access-cli.md).
