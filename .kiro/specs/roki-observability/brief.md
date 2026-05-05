---
refs:
  id: brief:roki-observability
  kind: brief
  title: "roki-observability Brief"
  spec: roki-observability
---

# Brief: roki-observability

## Problem
"What is roki currently doing?" requires tailing logs and grepping. A TUI is wanted; a TUI needs a state-source.

## Current State
- roki-mvp has structured logs via tracing, but no programmatic surface for state.
- TUI is roadmapped from monorail but never built.

## Desired Outcome
- An optional HTTP server (axum) exposing roki's in-memory orchestrator state as JSON over loopback.
- A ratatui TUI client that consumes the JSON API and renders: active worker list, per-issue state + last event, escalation queue, token usage, rate-limit snapshots.
- The daemon and TUI are separate processes (TUI optional, can be run on demand or kept open).
- API schema versioned under `/api/v1/` and defined in a single shared crate so external dashboards (future web UI) interop without per-tool variants.

## Approach
Add an axum HTTP server module to the daemon, gated by `server.port` (off by default, loopback bind). Endpoints: `GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh`. Build a separate ratatui binary (`roki-tui`) that reads from this API with a refresh loop. TUI provides: live state view, basic interactions (acknowledge escalation, request refresh). No state-mutating actions in v1.

## Scope
- **In**:
  - axum HTTP server module in roki daemon
  - Endpoints: `GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh`
  - Versioned JSON schema (`/api/v1/`) defined in a single shared crate
  - Loopback-only default bind, configurable
  - HTML / ANSI escaping for any agent-derived strings from day one
  - `roki-tui` ratatui binary: live state view, escalation acknowledgement, refresh button
  - macOS + Linux terminal compatibility (iTerm2 / Ghostty / WezTerm / Alacritty primary)

- **Out**:
  - Authentication / auth tokens (loopback-only; multi-user is out of scope)
  - Web UI (JSON API enables it; not building it here)
  - Mutating endpoints beyond `/refresh` (no cancel, no retry -- those need state-machine API contracts that should be designed separately)
  - Persistent metrics / time-series storage

## Boundary Candidates
- **HTTP API vs orchestrator core**: API is a read-only projection of orchestrator state; orchestrator does not depend on the API.
- **TUI binary vs daemon**: separate processes; TUI talks only to the JSON API.
- **Schema definition**: shared `serde` types between server and TUI client (single crate).

## Out of Boundary
- Web UI implementation.
- Mutating control plane (cancel, retry, reschedule).
- Authn / multi-user.

## Upstream / Downstream
- **Upstream**: roki-mvp (orchestrator state is the data source).
- **Downstream**: future web UI; future control-plane spec.

## Existing Spec Touchpoints
- **Extends**: roki-mvp (adds optional HTTP server module).
- **Adjacent**: none.

## Constraints
- JSON schema versioned under `/api/v1/` and centralized in a single shared crate so external dashboards interop on a stable contract.
- All agent-derived strings must be HTML-escaped + ANSI-stripped before rendering.
- Default bind must be loopback only; document the exposure risk if the user changes it.
- TUI must degrade gracefully on Terminal.app (informational warning about RGB color limits).
