---
refs:
  id: design:roki-observability
  kind: design
  title: "roki-observability Design"
  spec: roki-observability
  implements:
    - requirements:roki-observability
---

# Design Document

## Overview

**Purpose**: Two artifacts: (1) an optional axum HTTP server module compiled into the `roki` daemon binary, gated by `WORKFLOW.md` `server.port` and bound to loopback by default; (2) a separate `roki-tui` ratatui binary in the same Cargo workspace that consumes the HTTP API on a refresh loop. The API is a read-only projection of orchestrator in-memory state plus one mutating endpoint (`POST /api/v1/refresh`) that nudges the tracker poller — no cancel, retry, or reschedule. The JSON schema is versioned under `/api/v1/` and centralized in a single shared crate so server, TUI, and any future external dashboard consume one stable contract.

**Users**: Solo developer or small team operator running roki as a daemon. Future web-UI authors and external dashboards are downstream consumers of the same JSON schema.

**Impact**: Two seams added without disturbing roki-mvp: a `TransitionSubscriber` against the existing event bus, and an on-demand projection over the live state. The orchestrator core does not depend on the API. The daemon continues to function with the API disabled (the default).

### Goals
- Optional axum HTTP server module gated by `WORKFLOW.md` `server.port` (off by default), loopback-only by default.
- Three endpoints: `GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh` — stable JSON shape under the `/api/v1/` prefix.
- Day-one HTML escaping plus ANSI stripping of every agent-derived and Linear-derived string field, on both API and TUI sides (defense in depth).
- A `roki-tui` ratatui binary that renders active workers, per-issue state, last lifecycle event, escalation queue, token usage, rate-limit snapshots; supports local escalation acknowledgement and a manual refresh action.
- Single shared `roki-api-types` crate so server and TUI cannot drift on schema.
- Reuse of roki-mvp's in-memory state model — no duplicated state structs.

### Non-Goals
- Authentication, authorization, TLS, or multi-user access. Loopback-only assumption.
- Web UI implementation. The JSON API enables it; nothing here builds it.
- Mutating control plane beyond `/refresh`. No cancel, retry, reschedule, pause, resume, or workspace operations.
- Persistent metrics, time-series storage, or historical event archives. Every snapshot is read live from in-memory state.
- Windows TUI support. macOS plus Linux only in v1.
- Persisted TUI session state across restarts beyond what is typed in the current session.

## Boundary Commitments

### This Spec Owns
- The `roki_api_types` shared crate: every `serde` type that crosses the HTTP boundary in either direction (snapshots, per-issue details, refresh acknowledgements, error envelopes), plus the projection contract from roki-mvp's in-memory state into those types.
- The `server/` module inside the daemon (under `crates/roki-daemon/src/server/`): axum router setup, axum `Server` binding, HTML-escape and ANSI-strip layer for every outbound string field, request-scoped tracing layer, in-memory request counter, refresh-debounce coordinator, projection assembler, mapper from roki-mvp's existing `EscalationEntry` to the API shape.
- The `roki-tui` binary (under `crates/roki-tui/`): ratatui app loop, terminal setup and teardown, refresh loop against the API, key-binding map, local-only escalation acknowledgement state, terminal-capability detection and graceful degradation, status-bar error reporting.
- The `server.*` extension block parsed from the existing `WorkflowPolicy::raw_unknowns` blob (the `extension.server.*` namespace is already reserved by roki-mvp's `WorkflowPolicy`; this spec only adds parsing, not schema registration).
- Documentation updates to `SPEC.md` and `WORKFLOW.example.md` covering the API surface, the `server.*` config block, and the loopback-bind security note.

### Preconditions (additive extensions to roki-mvp's already-published surface)
The current `OrchestratorRead`/`IssueState` projection in `crates/roki-daemon/src/orchestrator/read.rs` is too thin for Req 2.2 / Req 3.1. This spec extends it additively (no shape changes to existing fields) before the projection assembler can fulfill the API contract. These extensions live in roki-mvp's source but exist solely to support roki-observability and are listed as task-0.x preconditions in tasks.md:
- Extend `IssueState` (and `ActorSnapshot`) with optional `repo` (informational only — one ticket maps to at most one repo per `roki-mvp` design.md `Multi-repo tickets (rejected by the orchestrator with outcome=needs_split)`), `workspace_path`, `permission_strategy`, `last_event` (a new `TransitionEventSummary`), `last_event_at`, `last_error`, `correlation_id`.
- Rename roki-mvp's internal `read::SnapshotResponse` to `OrchestratorSnapshot` to free the name for `roki_api_types::SnapshotResponse`.
- Document the `SubscriberHooks::subscribe` "register before serving" call sequence.

> **No `issue_by_repo_issue`**: the existing `OrchestratorRead::issue(id: &IssueId)` is sufficient. roki-mvp's 1-ticket-1-repo invariant means `IssueId` alone is unique; the per-issue endpoint takes `<issue>` only and resolves through `issue(id)`. No `repo` query parameter is supported.

### Out of Boundary
- Any change to roki-mvp's orchestrator state machine, the `WorkerState` enum, the `TransitionEvent` shape, the `WorkflowLoader` schema beyond adding a reserved extension key, or the per-issue worker lifecycle. The HTTP server module reads through stable interfaces and never writes back into orchestrator state.
- Any mutating Linear, GitHub, or filesystem effect. `POST /api/v1/refresh` calls a tracker-side nudge API that already lives in roki-mvp; this spec does not invent new tracker behavior.
- Authentication, authorization, audit logging beyond standard request logging, persistent storage of any kind, web UI, or mutating control plane endpoints.
- Any agent-side tool registration. The HTTP API is operator-facing; the agent does not see it.

### Allowed Dependencies
- roki-mvp's published extension surface (per `roki-mvp` Req 13.1 / 13.3 and `fr:12-extension-surface`): `TransitionSubscriber` registration via `SubscriberHooks::subscribe` (in `crates/roki-daemon/src/orchestrator/hooks.rs`) for transition events; the read-only `OrchestratorRead` trait (`snapshot()` / `issue()` / `escalation_queue()`) for in-memory state and escalation-queue projection; the `TrackerRefresh` trait (`nudge()`, cadence-cap and 429-backoff aware) for out-of-cycle poll requests. The `OrchestratorRead` projection (`IssueState`) is extended additively with new optional fields via the precondition tasks; the trait method set is unchanged. All dependencies are stable, published in roki-mvp, and consumed read-only / nudge-only by this spec.
- axum 0.7+ and tower for HTTP routing and middleware. axum is already a roki-mvp dependency for the Linear webhook receiver, which keeps the dependency footprint flat.
- ratatui plus crossterm for the TUI; reqwest for the TUI's HTTP client (already in the workspace for Linear); serde and serde_json for shared types.
- A minimal HTML-escape crate (e.g. `html-escape`) and an ANSI-stripping crate (e.g. `strip-ansi-escapes`) consumed in the server module and the TUI for defense in depth.

### Revalidation Triggers
- Any change to roki-mvp's `WorkerState`, `TransitionEvent`, or `NormalizedIssue` shape — the projection in `roki_api_types` must be updated and the field documentation in SPEC.md re-checked.
- Any change to the `Orchestrator::subscribe` contract or to the orchestrator's in-memory state read interface — the `server` module's projection assembler must be re-verified.
- Any change to roki-mvp's tracker refresh-nudge interface — the `POST /api/v1/refresh` handler must be re-checked.
- Any new agent-derived or Linear-derived string field added to roki-mvp's state — the API serialization must apply the escape/strip layer to it on day one.
- Adding a new mutating endpoint or changing the loopback-only default — both require revisiting the security note in `SPEC.md` and `WORKFLOW.example.md` plus an explicit acceptance criterion in this spec.
- Adding authn/authz in a future spec — the loopback-only default must be re-evaluated and the security note in `SPEC.md` updated.

## Architecture

### Architecture Pattern & Boundary Map

```mermaid
graph TB
    Operator[Operator]
    Tui[roki-tui binary]

    subgraph Daemon[roki daemon]
        Cli[CLI shell]
        Workflow[WorkflowLoader]
        Orchestrator[Orchestrator core]
        EscQ[Orchestrator escalation queue owned by roki-mvp]
        EventBus[Transition event bus]
        Tracker[Tracker adapter]

        subgraph Server[server module optional]
            Router[axum router]
            Snapshot[Snapshot assembler]
            Escape[Escape and strip layer]
            EventCache[Recent event cache]
            EscView[EscalationQueueView mapper]
            RefreshDebounce[Refresh debounce]
            Subscriber[TransitionSubscriber adapter]
        end
    end

    Cli --> Workflow
    Workflow --> Server
    Orchestrator --> EventBus
    Orchestrator --> EscQ
    EventBus --> Subscriber
    Subscriber --> EventCache
    Server --> Snapshot
    Snapshot --> Orchestrator
    Snapshot --> EventCache
    Snapshot --> EscView
    EscView --> EscQ
    Snapshot --> Escape
    Router --> RefreshDebounce
    RefreshDebounce --> Tracker
    Tui --> Router
    Operator --> Tui
```

**Architecture Integration**:
- **Selected pattern**: Read-only projection plus thin mutating-nudge. The HTTP server lives in its own module; it depends on roki-mvp's orchestrator and tracker interfaces but the orchestrator does not depend on the server. The TUI is a separate process talking only over the JSON API.
- **Domain boundaries**: `roki_api_types` (shared `serde` schema) vs `server/` (HTTP module inside the daemon) vs `roki-tui` (separate binary). The orchestrator and tracker live in roki-mvp; the server module imports them through their published trait interfaces.
- **Existing patterns preserved**: axum is already in the daemon for the Linear webhook receiver; tracing is already the structured-logging pipeline; `WorkflowLoader` already publishes the `extension.*` reserved namespace for downstream specs.
- **New components rationale**: A small `TransitionSubscriber` cache exists to keep the most recent N lifecycle events per issue without re-walking the orchestrator on every request; this trades a bounded ring buffer per active issue for snapshot-time work.
- **Steering compliance**: Rust 2024, tokio runtime, no SQLite or persistent store, macOS plus Linux only, kiro skills unaffected.

### Technology Stack

| Layer | Choice / Version | Role in Feature | Notes |
|-------|------------------|-----------------|-------|
| HTTP server | axum 0.7+, tower 0.5+ | Router, middleware, request lifecycle | Already a daemon dependency |
| HTTP client (TUI) | reqwest 0.12+ | TUI fetches `/api/v1/state` and posts `/refresh` | Reuses workspace dependency; rustls TLS unused for loopback but kept for non-loopback opt-in |
| TUI framework | ratatui 0.26+, crossterm 0.27+ | Terminal rendering, key handling, terminal mode | Selected for active maintenance and macOS plus Linux focus |
| Shared types | serde 1.x, serde_json 1.x | `roki-api-types` crate | No additional dependencies needed |
| Escape / strip | html-escape 0.2+, strip-ansi-escapes 0.2+ | HTML-escape and ANSI-strip layer | Applied in server projection and again in TUI render |
| Logging | tracing, tracing-subscriber | Server request logs and TUI startup logs | Reuses existing pipeline; TUI logs to stderr only |
| CLI | clap 4.x | TUI binary CLI | Workspace dependency; daemon CLI is unchanged here |

> TUI uses crossterm rather than termion for macOS plus Linux uniformity.

## File Structure Plan

### Directory Structure

```
SPEC.md                                  # Updated with /api/v1/* contract and server.* config block
WORKFLOW.example.md                      # Updated with example server.* block and exposure note
Cargo.toml                               # Workspace already exists (resolver = "3"); gains roki-api-types and roki-tui members
crates/
├── roki-daemon/                         # Existing daemon crate (roki-mvp owned)
│   ├── Cargo.toml                       # Gains path dep on roki-api-types
│   └── src/
│       ├── main.rs                      # Modified: conditionally spawn server::ServerModule::run
│       └── server/                      # NEW module added by this spec
│           ├── mod.rs                   # ServerModule entrypoint, optional spawn, axum::Server bind
│           ├── router.rs                # Route table for /api/v1/state, /api/v1/<issue>, /api/v1/refresh
│           ├── projection.rs            # Project orchestrator state into roki_api_types shapes
│           ├── escape.rs                # HTML-escape plus ANSI-strip helper, used by projection
│           ├── event_cache.rs           # Per-issue ring buffer of recent lifecycle events; subscriber writes here
│           ├── escalation_queue_view.rs # Mapper from roki-mvp's EscalationEntry to roki_api_types::EscalationEntry
│           ├── refresh.rs               # Refresh debounce coordinator, calls tracker nudge
│           ├── logging.rs               # tracing layer + HttpRequestCounter
│           └── config.rs                # Parse server.* block from WorkflowPolicy::raw_unknowns
├── roki-api-types/
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                       # Re-exports, version constant, top-level error envelope
│       ├── snapshot.rs                  # SnapshotResponse, ServerBlock, RepoSummary, AggregateUsage
│       ├── issue.rs                     # IssueDetailResponse, RecentEventEntry, ProjectionState
│       ├── refresh.rs                   # RefreshRequest, RefreshAccepted
│       └── error.rs                     # ApiError envelope plus ApiErrorCode
└── roki-tui/
    ├── Cargo.toml
    └── src/
        ├── main.rs                      # Binary entry, clap, runtime bootstrap
        ├── app.rs                       # AppState, refresh loop, event loop coordinator
        ├── api_client.rs                # Thin reqwest wrapper for the three endpoints
        ├── render.rs                    # Layout: header, worker table, escalation panel, status bar
        ├── input.rs                     # Key handling, debounce, escalation acknowledgement
        ├── terminal_caps.rs             # Terminal-capability detection and palette fallback
        └── sanitize.rs                  # ANSI-strip and control-character filter (defense in depth)
```

### Modified Files
- `Cargo.toml` (workspace root) — append `crates/roki-api-types` and `crates/roki-tui` to existing `[workspace] members` (the workspace already exists with `resolver = "3"` and `crates/roki-daemon`, `crates/roki-doctools`).
- `crates/roki-daemon/Cargo.toml` — add path dependency on `roki-api-types`.
- `crates/roki-daemon/src/main.rs` — at startup, after the workflow watcher reports a valid policy, conditionally spawn `server::ServerModule::run` when `policy.extension.server.port` is set.
- `crates/roki-daemon/src/orchestrator/hooks.rs` — no change to the trait; the server module registers an `Arc<dyn TransitionSubscriber>` via `SubscriberHooks::subscribe` like any other subscriber.
- `crates/roki-daemon/src/orchestrator/read.rs` — additive only (precondition 0.x): extend `IssueState` / `ActorSnapshot` with optional projection fields; rename internal `SnapshotResponse` to `OrchestratorSnapshot`.
- `crates/roki-daemon/src/orchestrator/state.rs` — additive only (precondition 0.x): introduce `TransitionEventSummary` (or place adjacent in `read.rs`).
- `crates/roki-daemon/src/workflow/schema.rs` — no schema-level change required; `extension.server.*` is already a reserved namespace round-tripped through `WorkflowPolicy::raw_unknowns`. The server module parses from that blob directly.
- `SPEC.md` — add a section documenting the `/api/v1/*` contract (field-by-field response shapes, status codes, error envelope, version-stability rules), the `server.*` config block, and the loopback-only default.
- `WORKFLOW.example.md` — add a commented-out example `server.*` block with the exposure warning.

> Splitting `projection.rs` (snapshot assembly) from `escape.rs` (sanitization) keeps the escape/strip pass reusable from the per-issue endpoint.

## System Flows

### Daemon-side request handling (state and per-issue)

```mermaid
sequenceDiagram
    participant Client as TUI or external client
    participant Router as axum router
    participant Projection as Snapshot assembler
    participant Cache as Event cache
    participant Orch as Orchestrator state read
    participant Escape as Escape and strip

    Client->>Router: GET /api/v1/state
    Router->>Projection: build_snapshot
    Projection->>Orch: read live state
    Projection->>Cache: read recent events per issue
    Projection->>Escape: apply on every agent or linear string
    Escape-->>Projection: sanitized fields
    Projection-->>Router: SnapshotResponse
    Router-->>Client: 200 application json
```

> The snapshot assembler performs three sub-reads in a fixed order — orchestrator state map, per-issue event cache, escalation queue — and stamps `snapshot_at` after the third sub-read completes. Readers may observe entries whose `state`, last lifecycle event, and `EscalationEntry` reflect adjacent moments rather than one instant; the bound between the earliest and the latest sub-read is the snapshot drift bound (≤50ms under nominal load on a developer-class machine, documented in `SPEC.md`). The cache is not the source of truth; it is a bounded ring fed by the transition subscriber so the snapshot does not have to walk full history each request.

### Refresh-nudge handling

```mermaid
sequenceDiagram
    participant Client as TUI or external client
    participant Router as axum router
    participant Debounce as Refresh debounce
    participant Tracker as Tracker adapter

    Client->>Router: POST /api/v1/refresh
    Router->>Debounce: request
    alt within minimum interval
        Debounce-->>Router: coalesced; reuse pending nudge
        Router-->>Client: 202 RefreshAccepted coalesced true
    else allowed now
        Debounce->>Tracker: nudge
        Tracker-->>Debounce: scheduled at next tick
        Debounce-->>Router: accepted; earliest fire time
        Router-->>Client: 202 RefreshAccepted coalesced false
    else tracker in 429 backoff
        Debounce->>Tracker: nudge
        Tracker-->>Debounce: deferred until backoff window elapses
        Debounce-->>Router: accepted; earliest fire time
        Router-->>Client: 202 RefreshAccepted coalesced false earliest_fire_at set
    end
```

### TUI refresh loop

```mermaid
sequenceDiagram
    participant Operator
    participant Tui as roki-tui
    participant Api as Daemon HTTP API

    Operator->>Tui: launch roki-tui --url
    Tui->>Api: GET /api/v1/state
    Api-->>Tui: SnapshotResponse
    Tui->>Tui: render
    loop refresh cadence
        Tui->>Api: GET /api/v1/state
        alt 2xx
            Api-->>Tui: SnapshotResponse
            Tui->>Tui: re-render
        else non-2xx
            Api-->>Tui: ApiError
            Tui->>Tui: status bar shows error keep last good frame
        end
    end
    Operator->>Tui: refresh key
    Tui->>Api: POST /api/v1/refresh
    Api-->>Tui: 202 RefreshAccepted
    Tui->>Tui: status bar shows accepted
    Operator->>Tui: quit key
    Tui->>Tui: restore terminal exit zero
```

## Requirements Traceability

| Requirement | Summary | Components | Interfaces | Flows |
|-------------|---------|------------|------------|-------|
| 1.1, 1.2, 1.3, 1.4, 1.5, 1.6 | Optional gated server, loopback default, hot-reload note | ServerModule, ServerConfig, WorkflowSchemaExt | `server.*` policy parse, axum bind | n/a |
| 2.1, 2.2, 2.3, 2.4, 2.5, 2.6 | `GET /api/v1/state` snapshot | Router, Projection, EventCache, EscapeStrip | `SnapshotResponse` | State request flow |
| 3.1, 3.2, 3.3, 3.4, 3.5, 3.6 | `GET /api/v1/<issue>` per-issue detail | Router, Projection, EventCache, EscapeStrip | `IssueDetailResponse` | State request flow (per-issue variant) |
| 4.1, 4.2, 4.3, 4.4, 4.5 | `POST /api/v1/refresh` | Router, RefreshDebounce, Tracker (read of refresh nudge) | `RefreshAccepted` | Refresh-nudge flow |
| 5.1, 5.2, 5.3, 5.4, 5.5 | Stable versioned JSON schema in shared crate | roki_api_types crate, ServerModule, TuiApiClient | `API_VERSION`, response `api_version` field, `/api/v1/` URL prefix | n/a |
| 6.1, 6.2, 6.3, 6.4, 6.5 | Escape and strip on every agent or Linear string | EscapeStrip (server side), TUI sanitize (defense in depth) | escape and strip helpers | State request flow |
| 7.1, 7.2, 7.3, 7.4 | Loopback default and exposure documentation | ServerConfig, ServerModule, SPEC.md, WORKFLOW.example.md | bind-host validation | n/a |
| 8.1, 8.2, 8.3, 8.4, 8.5, 8.6 | `roki-tui` binary basic UX | TuiApp, ApiClient, Render, Input | clap CLI, refresh loop | TUI refresh loop |
| 9.1, 9.2, 9.3, 9.4 | TUI escalation acknowledgement | TuiApp, Render, Input | Local AckState | TUI refresh loop |
| 10.1, 10.2, 10.3, 10.4 | TUI refresh action | TuiApp, ApiClient, Input | `POST /api/v1/refresh` | TUI refresh loop |
| 11.1, 11.2, 11.3, 11.4, 11.5 | Terminal compatibility and graceful degradation | TerminalCaps, Render | crossterm capability probing | n/a |
| 12.1, 12.2, 12.3, 12.4, 12.5 | Shared API types crate | roki_api_types crate | Type definitions | n/a |
| 13.1, 13.2, 13.3, 13.4, 13.5 | Read-only projection, no duplicated state | ServerModule, Projection, Subscriber | TransitionSubscriber registration | State request flow |
| 14.1, 14.2, 14.3, 14.4, 14.5 | Server and TUI logging | Logging layer (server side), TUI startup log | tracing fields, request counter | State request flow |
| 15.1, 15.2, 15.3, 15.4, 15.5 | `server.*` configuration in `WORKFLOW.md` | ServerConfig, WorkflowSchemaExt | `WorkflowPolicy.extension.server` | n/a |

## Components and Interfaces

| Component | Domain/Layer | Intent | Req Coverage | Key Dependencies (P0/P1) | Contracts |
|-----------|--------------|--------|--------------|--------------------------|-----------|
| ApiTypes (`roki_api_types`) | Shared types | One source of truth for every JSON request/response shape | 5.1, 5.2, 5.3, 5.4, 5.5, 12.1, 12.2, 12.3, 12.4, 12.5 | serde (P0) | State |
| ServerModule | Server/lifecycle | Optional axum server; bind, spawn, register subscriber, hold shared state | 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 7.1, 7.2, 13.1, 13.5 | WorkflowLoader (P0), Orchestrator (P0), Tracker (P0), tokio (P0), axum (P0) | Service |
| ServerConfig | Server/config | Parse `server.*` from `WorkflowPolicy.extension`, validate, normalize | 1.1, 1.4, 1.5, 7.1, 7.2, 15.1, 15.2, 15.3, 15.4, 15.5 | WorkflowLoader (P0) | State |
| Router | Server/HTTP | axum router; routes for state, per-issue, refresh; default headers | 2.1, 2.5, 3.1, 3.5, 4.1 | axum (P0), Projection (P0), RefreshCoordinator (P0) | API |
| Projection | Server/projection | Map orchestrator state and event cache into `roki_api_types` shapes | 2.1, 2.2, 2.3, 2.6, 3.1, 3.3, 3.4, 5.1, 5.2, 5.4, 13.2, 13.3, 13.4 | Orchestrator (P0), EventCache (P0), EscapeStrip (P0) | Service, State |
| EscapeStrip | Server/projection | HTML-escape plus ANSI-strip every agent-derived and Linear-derived string field | 2.4, 3.4, 6.1, 6.2, 6.3, 6.5 | html-escape (P0), strip-ansi-escapes (P0) | Service |
| EventCache | Server/projection | Bounded per-issue ring buffer fed by TransitionSubscriber; supplies the per-issue detail endpoint's recent-event log only | 2.2, 3.1, 3.6, 13.1 | Orchestrator EventBus (P0) | State |
| EscalationQueueView | Server/projection | Pure mapper from roki-mvp's `orchestrator::escalation::EscalationEntry` (read via `OrchestratorRead::escalation_queue()`) to `roki_api_types::EscalationEntry`; maps `EscalationKind` to the snake_case `failure_reason` discriminator from `fr:14-operator-notifications`. No state of its own — roki-mvp owns the queue | 2.1, 13.1, 13.2 | OrchestratorRead (P0) | Service |
| TransitionSubscriberAdapter | Server/projection | Implements `TransitionSubscriber::on_transition` (sync, non-blocking, no `.await`, no veto method on the trait); writes one entry to EventCache per event; on terminal-state transition schedules a 2-second grace-window drop via `tokio::spawn + tokio::time::sleep`. Does NOT write to the escalation queue: roki-mvp's orchestrator already maintains its own escalation queue and the observability spec only reads through `OrchestratorRead::escalation_queue()` | 13.1, 13.2, 13.5 | Orchestrator (P0), EventCache (P0) | Event |
| RefreshCoordinator | Server/refresh | Debounce refresh requests, coalesce within minimum interval, call tracker nudge | 4.1, 4.2, 4.3, 4.4, 4.5 | Tracker (P0) | Service |
| HttpLogging | Server/logging | tracing layer that emits per-request structured events with redaction reuse and increments per-endpoint counter | 14.1, 14.2, 14.3, 14.5 | tracing (P0) | Service |
| TuiApp | TUI | App loop, state holder, refresh-loop coordinator, key router | 8.2, 8.3, 8.4, 8.5, 9.1, 9.3, 10.1, 10.2 | ApiClient (P0), Render (P0), Input (P0), tokio (P0) | Service, State |
| TuiApiClient | TUI | reqwest wrapper for `GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh` | 8.2, 8.3, 8.6, 10.1, 10.3 | reqwest (P0), ApiTypes (P0) | API |
| TuiRender | TUI | Layout, color palette resolution, status bar, glyph table | 8.4, 9.2, 11.1, 11.2, 11.4 | ratatui (P0), TerminalCaps (P0) | Service |
| TuiInput | TUI | Key handling, refresh debounce, ack/quit/refresh keys | 8.5, 9.1, 10.1, 10.4 | crossterm (P0) | Service |
| TerminalCaps | TUI | Detect 24-bit RGB support, fall back palette, emit one-time notice | 11.1, 11.2, 11.3, 11.4, 11.5 | crossterm (P0) | State |
| TuiSanitize | TUI | ANSI-strip plus control-char filter for every string used in render | 6.4 | strip-ansi-escapes (P0) | Service |
| WorkflowSchemaExt | Workflow integration | Register `server.*` keys as additive under `extension.*` | 1.6, 15.1, 15.2, 15.3, 15.4 | WorkflowLoader / Schema (P0) | State |
| SpecRootUpdate | Documentation | Update `SPEC.md` and `WORKFLOW.example.md` with API contract (field-by-field), config block, exposure note | 5.4, 7.3, 15.5 | n/a | n/a |

### Shared API types

#### ApiTypes (`roki_api_types`)

| Field | Detail |
|-------|--------|
| Intent | Single source of truth for every JSON shape that crosses the HTTP boundary |
| Requirements | 5.1, 5.2, 5.3, 5.4, 5.5, 12.1, 12.2, 12.3, 12.4, 12.5 |

**Responsibilities & Constraints**
- All structs derive `Serialize` and `Deserialize`; field names follow the roki naming conventions documented in `SPEC.md` (snake_case, stable across patch versions, additions are additive only).
- One ticket maps to at most one repo (per `roki-mvp` design.md `Multi-repo tickets (rejected by the orchestrator with outcome=needs_split)`). Per-issue shapes carry `issue: String` as the unique identifier and `repo: String` as an informational field showing which repo the worktree was created in. JSON consumers key on `issue` alone; `repo` is for display only.
- The crate must not depend on roki-mvp internals; the projection direction is daemon-internal-state -> ApiTypes, not the reverse.
- API version constant `API_VERSION: &str = "v1"` exposed at the crate root for the daemon and TUI to assert; every response body's `api_version` field equals this constant.

**Contracts**: Service [ ] / API [ ] / Event [ ] / Batch [ ] / State [x]

##### State / Type Sketch

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SnapshotResponse {
    pub api_version: String,                 // "v1"
    pub daemon: DaemonInfo,
    pub server: ServerBlock,
    pub repos: Vec<RepoSummary>,
    pub workers: Vec<WorkerSummary>,
    pub escalations: Vec<EscalationEntry>,
    pub aggregate_usage: AggregateUsage,
    pub aggregate_rate_limit: AggregateRateLimit,
    pub snapshot_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WorkerSummary {
    pub repo: String,
    pub issue: String,
    pub state: String,                       // ProjectionState string; documented fallback for unknown
    pub inactive_reason: Option<String>,     // Set when state == "inactive"; mirrors fr:04 Inactive(reason=...) discriminator
    pub last_event: Option<RecentEventSummary>,
    pub last_event_at: Option<chrono::DateTime<chrono::Utc>>,
    pub correlation_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IssueDetailResponse {
    pub api_version: String,
    pub repo: String,
    pub issue: String,
    pub state: String,
    pub inactive_reason: Option<String>,     // Set when state == "inactive"; mirrors fr:04 Inactive(reason=...) discriminator
    pub recent_events: Vec<RecentEventEntry>,
    pub last_error: Option<String>,
    pub permission_strategy: String,
    pub workspace_path: String,
    pub truncated: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerBlock {
    pub bind: String,
    pub port: u16,
    pub request_counters: BTreeMap<String, u64>,        // satisfies Req 14.5
    pub min_refresh_interval_seconds: u64,              // TUI reads this for client-side debounce (Req 10.4)
    pub max_event_log_per_issue: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RecentEventSummary {
    pub kind: String,                                   // snake_case TransitionTrigger discriminator
    pub message: Option<String>,                        // escaped/stripped preview
    pub at: chrono::DateTime<chrono::Utc>,
    pub correlation_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EscalationEntry {
    pub repo: String,
    pub issue: String,
    pub failure_reason: String,              // fr:04 Inactive.reason discriminator value (e.g. "orchestrator_crash")
    pub raised_at: chrono::DateTime<chrono::Utc>,
    pub correlation_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RefreshAccepted {
    pub api_version: String,
    pub coalesced: bool,
    pub earliest_fire_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiError {
    pub api_version: String,
    pub code: String,                        // e.g. "NOT_FOUND", "UNHEALTHY", "INTERNAL"
    pub message: String,
    pub details: Option<serde_json::Value>,
}
```

- Preconditions: every string field is already escaped and stripped before serialization; the projection in the daemon is responsible for ensuring this.
- Postconditions: schema field names are stable across patch versions; additions are additive only.
- Invariants: `api_version` is always set to `"v1"`.

### Server module

#### ServerModule

| Field | Detail |
|-------|--------|
| Intent | Optional axum HTTP server: read `server.*` config, bind socket, register transition subscriber, drive shutdown |
| Requirements | 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 7.1, 7.2, 13.1, 13.5 |

**Responsibilities & Constraints**
- Read `WorkflowPolicy.extension.server` (parsed by `ServerConfig`); if `port` is unset, do nothing and log info-level "API disabled".
- Register a `TransitionSubscriberAdapter` with the orchestrator before binding the socket; if subscription fails, log error and continue without the API.
- Bind axum on `(bind_host, port)`; on bind failure, log error with port and underlying error and do not retry in v1.
- Hold an `Arc<AppState>` containing references to the orchestrator state-read interface, the event cache, the refresh coordinator, and a per-endpoint request counter.
- On shutdown signal, drain in-flight requests within the bounded shutdown window and stop the listener cleanly.

**Dependencies**
- Inbound: daemon main task — calls `ServerModule::run` after `WorkflowLoader` reports a valid policy (P0).
- Outbound: WorkflowLoader — reads `policy.extension.server` (P0).
- Outbound: Orchestrator — registers subscriber and reads live state (P0).
- Outbound: Tracker adapter — calls refresh-nudge from the refresh coordinator (P0).

**Contracts**: Service [x] / API [ ] / Event [ ] / Batch [ ] / State [ ]

##### Service Interface (Rust trait sketch)

```rust
pub struct ServerModule {
    config: ServerConfig,
    orchestrator: Arc<dyn OrchestratorRead>,
    tracker: Arc<dyn TrackerRefresh>,
    event_cache: Arc<EventCache>,
    request_counter: Arc<HttpRequestCounter>,
    subscriber_hooks: Arc<SubscriberHooks>,
}

impl ServerModule {
    pub async fn run(self, shutdown: ShutdownSignal) -> Result<(), ServerError>;
}

// roki-mvp surface (already published; see crates/roki-daemon/src/orchestrator/{read,hooks}.rs)
pub trait OrchestratorRead: Send + Sync {
    fn snapshot(&self) -> OrchestratorSnapshot;                                          // renamed from internal SnapshotResponse (precondition 0.x)
    fn issue(&self, id: &IssueId) -> Option<IssueState>;                                 // existing; sufficient because 1 ticket = 1 repo
    fn escalation_queue(&self) -> Vec<EscalationEntry>;                                  // existing
}

// Subscribe lives on a separate type, not on OrchestratorRead.
impl SubscriberHooks {
    pub fn subscribe(&self, sub: Arc<dyn TransitionSubscriber>) -> SubscriptionHandle;   // existing
}

pub trait TrackerRefresh: Send + Sync {
    fn nudge(&self) -> NudgeOutcome;        // synchronous, non-blocking; reports earliest fire time
}
```

- Preconditions: `WorkflowLoader` has produced a valid policy; orchestrator and tracker handles are constructed.
- Postconditions: when `port` is set, an axum task is running and serving the documented endpoints; when `port` is unset, no socket is bound.
- Invariants: the server module never writes to orchestrator state; `POST /api/v1/refresh` flows through `TrackerRefresh::nudge` only.

**Implementation Notes**
- Integration: `tokio::spawn` an axum task; bind via `axum::serve(TcpListener::bind(...))`; pass shutdown via `with_graceful_shutdown`.
- Validation: bind-host classification (loopback vs not) is in `ServerConfig`; the warn-level log on non-loopback bind fires from `ServerModule::run` before serving begins.
- Risks: orchestrator read API drift. Mitigation: `OrchestratorRead` is a thin trait the orchestrator implements; downstream changes require updating one trait method, not the projection.

#### ServerConfig

| Field | Detail |
|-------|--------|
| Intent | Parse and validate the `server.*` block from `WorkflowPolicy.extension` |
| Requirements | 1.1, 1.4, 1.5, 7.1, 7.2, 15.1, 15.2, 15.3, 15.4, 15.5 |

**Responsibilities & Constraints**
- Accept absence of the entire `server` block as the explicit "API disabled" state.
- Validate `port` (1..=65535), `bind` (parse as `IpAddr`; default to `127.0.0.1`), `min_refresh_interval_seconds` (>= 1, with documented default), `max_event_log_per_issue` (>= 1, with documented default).
- Classify `bind` as loopback (`127.0.0.0/8`, `::1/128`) versus non-loopback so the warn-level log can fire correctly.
- Surface validation errors as a typed enum that names the offending key.
- **Hot-reload semantics** (per `fr:02-configuration` and Req 15.3a): when invoked from a hot-reload of `WORKFLOW.md`, validation failure must not crash the daemon — the previously loaded `WorkflowPolicy` (and the running `ServerModule`, if any) remain in effect, the offending key is logged at error level, and the loader retains the last-known-good policy. v1 does not perform runtime re-bind: changes to `port` / `bind` apply only on next daemon restart.

**Contracts**: Service [ ] / API [ ] / Event [ ] / Batch [ ] / State [x]

```rust
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub port: u16,
    pub bind: IpAddr,                        // default 127.0.0.1
    pub min_refresh_interval: Duration,      // default 5 seconds
    pub max_event_log_per_issue: usize,      // default 50
}

#[derive(Debug, thiserror::Error)]
pub enum ServerConfigError {
    #[error("server.port out of range: {0}")]
    InvalidPort(i64),
    #[error("server.bind is not a valid IP address: {0}")]
    InvalidBind(String),
    #[error("server.{key} must be at least 1 (got {value})")]
    OutOfRange { key: &'static str, value: i64 },
}
```

#### Projection

| Field | Detail |
|-------|--------|
| Intent | Build `SnapshotResponse` and `IssueDetailResponse` from the live orchestrator state and the event cache, applying escape/strip per field |
| Requirements | 2.1, 2.2, 2.3, 2.6, 3.1, 3.3, 3.4, 5.1, 5.2, 5.4, 13.2, 13.3, 13.4 |

**Responsibilities & Constraints**
- Perform the snapshot read in a fixed three-step order: (1) call `OrchestratorRead::snapshot()` (which itself acquires the orchestrator state-map `RwLock` read guard, clones the per-issue projection, then snapshots the escalation queue), (2) read the per-issue event cache once per issue, (3) record `snapshot_at = Utc::now()` after the third sub-read returns. The order is fixed so the bound between the earliest and the latest sub-read is deterministic; the bound is the snapshot drift bound (≤50ms under nominal load on a developer-class machine, documented in `SPEC.md`). The bound is asserted by an integration test that drives concurrent transitions during snapshot assembly.
- Map roki-mvp's `WorkerState` (`Pending`, `Active`, `Backoff`, `Inactive`, `Cleaning` per `fr:04-state-machine-and-recovery`) to the documented `ProjectionState` strings: `Pending → "pending"`, `Active → "active"`, `Backoff → "backoff"`, `Inactive(_) → "inactive"`, `Cleaning → "cleaning"`. Any future variant maps to `"unknown"` with the original variant name preserved in the response `details` field.
- When `WorkerState == Inactive(reason)`, populate `WorkerSummary.inactive_reason` and `IssueDetailResponse.inactive_reason` with the raw discriminator string (snake_case, matching the `EscalationEntry.failure_reason` value); `None` for every other state.
- Apply `EscapeStrip::apply` to every string field carrying agent-derived or Linear-derived content (issue title, description, last-event message, last-error string, label values, tool-result preview).
- For per-issue detail, look up the unique entry via `OrchestratorRead::issue(id)`. There is no multi-repo disambiguation — one ticket maps to at most one repo per `roki-mvp` design.md (multi-repo tickets are rejected by the orchestrator with `outcome=needs_split`).
- Return `ApiError { code: "UNHEALTHY", ... }` with a 503 status if `OrchestratorRead::snapshot` reports the orchestrator is partially initialized.

**Contracts**: Service [x] / API [ ] / Event [ ] / Batch [ ] / State [x]

#### EscapeStrip

| Field | Detail |
|-------|--------|
| Intent | One reusable helper that HTML-escapes and ANSI-strips a string |
| Requirements | 2.4, 3.4, 6.1, 6.2, 6.3, 6.5 |

**Responsibilities & Constraints**
- `EscapeStrip::apply(input: &str) -> String`: strip ANSI escape sequences first, then HTML-escape the result; on invalid UTF-8 or unrecoverable input, return a fixed sanitized placeholder marker (`"[redacted-invalid]"`) and emit a structured tracing event with the source field name.
- Used by `Projection` for every agent-derived or Linear-derived string field.

#### EventCache and TransitionSubscriberAdapter

| Field | Detail |
|-------|--------|
| Intent | Per-issue bounded ring of recent lifecycle events fed by the orchestrator's transition subscriber so the per-issue detail endpoint avoids walking history. **Scope: detail endpoint only — not used to derive `SnapshotResponse.escalations`.** |
| Requirements | 2.2, 3.1, 3.6, 13.1 |

**Responsibilities & Constraints**
- One ring buffer per active `(repo: Option<String>, issue: IssueId)` keyed by orchestrator key; capacity from `ServerConfig::max_event_log_per_issue`.
- `TransitionSubscriberAdapter` registers via `SubscriberHooks::subscribe` and writes one entry per `TransitionEvent` into EventCache. The trait method `on_transition(&self, event: &TransitionEvent)` is **synchronous and non-blocking** (per `crates/roki-daemon/src/orchestrator/hooks.rs`); the adapter MUST NOT call `.await` and MUST NOT block on locks. The trait does NOT have a `veto` method. The adapter does NOT write to any escalation queue: roki-mvp's orchestrator already maintains its own escalation queue and the observability spec reads through `OrchestratorRead::escalation_queue()` only.
- The "latest lifecycle event" surfaced via `WorkerSummary.last_event` is the most recent state-machine transition regardless of origin. Phase-subprocess exit envelopes (`phase_complete` / `phase_nonclean` per `fr:18-worker-skill-workflow`) and orchestrator-stdin daemon directives (per `fr:19-orchestrator-session`) are observed only through the resulting `TransitionEvent`; the projection does not subscribe to either source directly.
- On worker terminal-state transition (`next == Cleaning` or removal), the adapter schedules a 2-second grace-window drop of the cache entry via `tokio::spawn` + `tokio::time::sleep(Duration::from_secs(2))` so that one final snapshot can still surface the terminal event before the entry is purged.

**Contracts**: Service [ ] / API [ ] / Event [x] / Batch [ ] / State [x]

#### EscalationQueueView (mapper, not a queue)

| Field | Detail |
|-------|--------|
| Intent | Pure mapper from roki-mvp's existing `orchestrator::escalation::EscalationEntry` to the API's `roki_api_types::EscalationEntry`. The observability spec does NOT own a parallel queue — roki-mvp's orchestrator owns the authoritative queue and exposes it via `OrchestratorRead::escalation_queue() -> Vec<EscalationEntry>`. |
| Requirements | 2.1, 13.1, 13.2 |

**Responsibilities & Constraints**
- `to_api_entry(e: &orchestrator::escalation::EscalationEntry) -> roki_api_types::EscalationEntry`: maps `EscalationKind` to the snake_case `failure_reason` discriminator from `fr:14-operator-notifications` (e.g. `orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`, `stall`, `retry_exhausted`, `fs_poison`, `orphan`, `allowlist_rejected`, `needs_split`, `spec_incomplete`, `needs_operator`); maps `repo: Option<String>` to `String` by emitting `""` when absent; renames `timestamp` to `raised_at` on the API side; passes through `correlation_id`.
- If `EscalationKind` lacks a 1:1 mapping for any reason listed in `fr:14`, extend the kind in roki-mvp additively (precondition task) rather than dropping the case silently.
- No state of its own; idempotent and pure.

**Contracts**: Service [x] / API [ ] / Event [ ] / Batch [ ] / State [ ]

#### RefreshCoordinator

| Field | Detail |
|-------|--------|
| Intent | Debounce `POST /api/v1/refresh` calls and translate them into tracker nudges |
| Requirements | 4.1, 4.2, 4.3, 4.4, 4.5 |

**Responsibilities & Constraints**
- Maintain a "next allowed nudge time" timestamp; a request inside the window returns `coalesced: true` reusing any pending nudge.
- A request outside the window invokes `TrackerRefresh::nudge` and returns `coalesced: false` plus the tracker-reported earliest fire time.
- All requests log at info level with the client address (already provided by axum's `ConnectInfo`) and the coalescing decision.

**Contracts**: Service [x] / API [ ] / Event [ ] / Batch [ ] / State [x]

#### Router and HTTP API contract

| Method | Endpoint | Request | Response | Errors |
|--------|----------|---------|----------|--------|
| GET | `/api/v1/state` | none | 200 `SnapshotResponse` (`Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`) | 503 `ApiError { code: "UNHEALTHY" }` |
| GET | `/api/v1/<issue>` | none | 200 `IssueDetailResponse` | 404 `ApiError { code: "NOT_FOUND" }`, 503 `ApiError { code: "UNHEALTHY" }` |
| POST | `/api/v1/refresh` | empty body or future `RefreshRequest` (reserved) | 202 `RefreshAccepted` | 503 `ApiError { code: "UNHEALTHY" }` |

> Headers `Content-Type: application/json; charset=utf-8` and `Cache-Control: no-store` are always set on JSON responses. Bodies are not logged in v1.

### TUI binary

#### TuiApp

| Field | Detail |
|-------|--------|
| Intent | The `roki-tui` app loop: drive refresh ticks, route key events, hold local UI state |
| Requirements | 8.2, 8.3, 8.4, 8.5, 9.1, 9.3, 10.1, 10.2 |

**Responsibilities & Constraints**
- On startup, parse CLI args (`--url`, `--refresh-interval-ms`), enter raw mode, hide the cursor, run the event loop until quit.
- Maintain `AppState`: latest `SnapshotResponse`, last error string, set of acknowledged escalation IDs (cleared when an escalation disappears from the snapshot), refresh-in-flight flag, last refresh timestamp, `effective_min_refresh: Duration`.
- **Debounce interval source** (Req 10.4): `effective_min_refresh` starts at the documented pre-first-fetch default of `Duration::from_millis(1000)`. After each successful `state()` fetch, the loop updates `effective_min_refresh = Duration::from_secs(snapshot.server.min_refresh_interval_seconds)`. The input layer's `Refresh` debounce reads this value via shared `AppState`; the input layer itself does not own the value.
- Concurrently run a refresh task that calls `TuiApiClient::state` at the configured cadence and a key-event task that drains crossterm events; merge into `AppState` via channels.
- On quit, restore the terminal mode, show the cursor, exit zero.

**Contracts**: Service [x] / API [ ] / Event [ ] / Batch [ ] / State [x]

#### TuiApiClient

| Method | Endpoint | Notes |
|--------|----------|-------|
| `state()` | `GET /api/v1/state` | Deserializes into `SnapshotResponse`; on non-2xx returns `ApiError`. |
| `issue(issue)` | `GET /api/v1/<issue>` | Reserved for v1.1 detail view; not required for the primary refresh loop. |
| `refresh()` | `POST /api/v1/refresh` | Returns `RefreshAccepted`; surfaces tracker-reported earliest fire time in the status bar. |

#### TuiRender

| Field | Detail |
|-------|--------|
| Intent | Lay out the terminal frame: header, worker table, escalation panel, status bar |
| Requirements | 8.4, 9.2, 11.1, 11.2, 11.4 |

**Responsibilities & Constraints**
- Use a fixed three-region layout: header (`daemon`, uptime, `aggregate_usage`, `aggregate_rate_limit`), main split (active workers table on the left, escalations on the right), status bar (last error, refresh state, terminal-capability notice).
- Distinguish acknowledged from unacknowledged escalations by both color and a non-color glyph (e.g. `*` for unacknowledged, `.` for acknowledged) so that Terminal.app users without RGB color still see the difference.
- Render only printable ASCII or commonly supported Unicode glyphs (no Sixel, no Kitty graphics protocol).

#### TerminalCaps

| Field | Detail |
|-------|--------|
| Intent | Detect 24-bit RGB support and pick a palette |
| Requirements | 11.1, 11.2, 11.3, 11.4, 11.5 |

**Responsibilities & Constraints**
- Probe `COLORTERM` (`truecolor`/`24bit`), `TERM_PROGRAM`, and crossterm's terminal capability hints at startup.
- Emit one informational status-bar notice on startup if RGB is unavailable; do not repeat it on subsequent ticks.
- On Windows, exit non-zero with a not-supported message.

#### TuiSanitize

Implementation note: the TUI re-strips ANSI escapes and filters control characters from every string it receives before rendering. The server already does this, but the TUI defends in depth so a future API change does not silently regress the trust boundary.

### Workflow integration

#### WorkflowSchemaExt

Implementation note: under roki-mvp's `WorkflowSchema`, register an additive sub-schema at `extension.server` containing `port`, `bind`, `min_refresh_interval_seconds`, `max_event_log_per_issue`. Unknown sibling keys remain accepted (additive-friendly). `ServerConfig::from_policy(policy: &WorkflowPolicy) -> Result<Option<ServerConfig>, ServerConfigError>` parses this block and returns `Ok(None)` when absent.

### Documentation

#### SpecRootUpdate

Implementation note: extend `SPEC.md` with three sections — the `/api/v1/*` JSON contract (field names, status codes, error envelope), the `server.*` configuration block, and a security note that loopback is the default and any non-loopback bind is unauthenticated and thus the operator's risk. `WORKFLOW.example.md` gains a commented-out example block:

```yaml
# server:
#   port: 7842
#   bind: "127.0.0.1"        # change at your own risk; no authn in v1
#   min_refresh_interval_seconds: 5
#   max_event_log_per_issue: 50
```

## Data Models

### Domain Model

The HTTP module has no persistent domain model. The runtime in-memory model adds two aggregates:

- **EventCacheRing**: per-`IssueId` bounded ring of `RecentEventEntry` values produced by the transition subscriber. Capacity is `ServerConfig.max_event_log_per_issue`. Dropped on terminal-state grace expiry.
- **RefreshDebounceState**: `next_allowed_at: Option<Instant>` plus a pending-nudge handle.

No on-disk state is added by this spec.

### Data Contracts & Integration

- HTTP boundary: every type in the `roki_api_types` crate. Requests have no bodies in v1 (refresh body is reserved for future use). Responses are JSON, UTF-8, no streaming.
- Event subscription: `TransitionSubscriber::on_transition` is the only producer of `EventCacheRing` writes; failure is isolated and logged.

## Error Handling

### Error Strategy

- The HTTP module surfaces every error to the caller as an `ApiError` envelope with a stable `code` string. Internal errors (panics, projection failures) are caught at the axum middleware and mapped to `code: "INTERNAL"` with a 500 status; the underlying error is logged through tracing with redaction.
- `TuiApp` never exits on a transient API error; instead the status bar shows the error and the refresh loop continues.
- Server startup errors (bind failure, config validation failure) do not crash the daemon; the orchestrator continues without the API and the operator sees a structured error event.

### Error Categories and Responses

- **Configuration errors** (`server.*` invalid): server does not start; daemon continues; structured error log identifies the offending key.
- **Bind errors** (port in use, permission denied): server does not start; daemon continues; structured error log identifies the port and underlying OS error.
- **Projection errors** (orchestrator unhealthy): 503 `UNHEALTHY` to caller; logged.
- **Routing errors** (not found): 404 with the documented `code`; not logged at error level.
- **Refresh errors** (tracker permanently unable): still 202 with `earliest_fire_at` set to `None` and `coalesced: false`; the tracker is responsible for recovering on its own backoff schedule.
- **TUI client errors** (non-2xx, network): status bar message; refresh loop continues.

### Monitoring

Every API request emits a structured tracing event with method, path, status, duration, client address, correlation id; bodies are never logged. A per-endpoint request counter is exposed under `SnapshotResponse.server` so the operator can confirm the API is being used.

## Testing Strategy

### Unit Tests
- `EscapeStrip::apply` strips `\x1b[31mhi\x1b[0m` to `hi`, HTML-escapes `<script>alert(1)</script>` to `&lt;script&gt;alert(1)&lt;/script&gt;`, and replaces invalid UTF-8 input with the documented `[redacted-invalid]` placeholder.
- `ServerConfig::from_policy` returns `Ok(None)` when the `server` block is absent, returns `Err(InvalidPort)` on `port: 0`, returns `Err(InvalidBind)` on a malformed IP, and accepts a valid block.
- `RefreshCoordinator` coalesces a burst of three calls within the configured minimum interval into one nudge and reports `coalesced: true` for the second and third.
- `TerminalCaps::detect` selects the truecolor palette when `COLORTERM=truecolor`, falls back to 256-color when unset, and reports the one-time notice.
- `Projection::build_snapshot` maps a stub orchestrator state plus stub event cache into a `SnapshotResponse` whose every agent-derived string field has been escaped and stripped (verified by injecting `\x1b[31m<b>` test fixtures).

### Integration Tests
- End-to-end `GET /api/v1/state` against an axum test server backed by stub `OrchestratorRead` and stub `EventCache`: assert 200, `Content-Type: application/json; charset=utf-8`, `Cache-Control: no-store`, response deserializes losslessly into `roki_api_types::SnapshotResponse` and the `api_version` field equals `"v1"`.
- End-to-end `GET /api/v1/<issue>` for an existing issue returns 200 with the expected detail; for a missing issue returns 404 `NOT_FOUND`. (No multi-repo ambiguity case: 1 ticket = 1 repo per `roki-mvp` design.md.)
- End-to-end `POST /api/v1/refresh` calls a fake `TrackerRefresh` whose first call succeeds and second call (within the minimum interval) is coalesced; the response bodies match.
- WORKFLOW.md hot-reload integration: changing `server.port` while the daemon runs does not change the listening port at runtime; a structured log event records the deferred-until-restart decision.
- TransitionSubscriberAdapter integration: a stub orchestrator emits ten transitions for an issue with a cache cap of five; the resulting `IssueDetailResponse.recent_events` length is five and `truncated` is true.
- Loopback-default bind integration: with no `bind` configured, the server binds 127.0.0.1; a request from 127.0.0.1 succeeds and a startup log event names the bind address.
- Non-loopback bind integration: with `bind: 0.0.0.0` configured in a test fixture, a warn-level log event fires once at startup and serving proceeds.

### E2E Tests
- TUI happy path: launch `roki-tui --url http://127.0.0.1:<port>` against the daemon's test fixture, observe the initial frame contains the active worker list and the escalation panel, press the refresh key and observe the status bar transition.
- TUI degraded-terminal path: run on a terminal probe that reports no truecolor; observe the one-time fallback notice in the status bar and a 256-color rendering.
- TUI quit path: press the documented quit key; observe terminal mode restored and exit code zero.

### Performance / Load (informational)
- `GET /api/v1/state` under a stub orchestrator with one hundred active issues completes within the snapshot drift bound (≤50ms) on a developer-class machine; an integration test drives concurrent transitions during snapshot assembly and asserts the bound holds.
- `roki-tui` reaches first-frame render within the documented startup window of 1 second on a developer-class machine against a loopback daemon.
- `POST /api/v1/refresh` debounce holds under a 100-rps burst from a stub client for one minute without leaking pending nudge handles.

## Optional Sections

### Security Considerations

- **No authn in v1**: the API has no authentication or authorization. Loopback-only is the contract. Binding to a non-loopback interface is opt-in and produces a warn-level startup log.
- **Untrusted strings**: every agent-derived and Linear-derived string is HTML-escaped and ANSI-stripped on the server side and again ANSI-stripped on the TUI side. Defense in depth on day one.
- **No body logging**: the HTTP layer logs metadata only (method, path, status, duration, client address). Agent-derived strings never enter the log path through the API.
- **Secret redaction**: the existing tracing redaction layer continues to apply to API logs.
- **No SSRF risk on the daemon side**: the API is read-only plus a single tracker-nudge call; the daemon never fetches arbitrary URLs on behalf of clients.

### Performance & Scalability

- Snapshot work is O(active_workers) in the projection plus O(max_event_log_per_issue) per active worker. With the documented defaults (low-tens of issues, 50 events per issue), each snapshot fits in well under a millisecond's worth of allocator and serialization work on a developer-class machine.
- The TUI refresh cadence default is 1000 ms; operators can override via CLI.
- The HTTP module shares the daemon's tokio runtime; no separate thread pool.
