# Brief: roki-observability

## Problem
A long-running daemon doing autonomous work needs an observability surface humans can scan in seconds. Without it, "what is roki currently doing?" requires tailing logs and grepping. The user explicitly wants a TUI; a TUI needs a state-source.

## Current State
- roki-mvp has structured logs via tracing, but no programmatic surface for state.
- Symphony has both a JSON HTTP API (`/api/v1/state`, `/api/v1/<issue>`, `POST /api/v1/refresh`) and an optional terminal / web dashboard. roki should adopt a similar two-layer split.
- TUI is roadmapped from monorail but never built.

## Desired Outcome
- An optional HTTP server (axum) exposing roki's in-memory orchestrator state as JSON over loopback.
- A ratatui TUI client that consumes the JSON API and renders: active worker list, per-issue state + last event, escalation queue, token usage, rate-limit snapshots.
- The daemon and TUI are separate processes (TUI optional, can be run on demand or kept open).
- API schema is symphony-compatible so future dashboard alternates (web UI) can interop.

## Approach
Add an axum HTTP server module to the daemon, gated by `server.port` (off by default, loopback bind). Endpoints mirror symphony's: `GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh`. Build a separate ratatui binary (`roki-tui`) that reads from this API with a refresh loop. TUI provides: live state view, basic interactions (acknowledge escalation, request refresh). No state-mutating actions in v1.

## Scope
- **In**:
  - axum HTTP server module in roki daemon
  - Endpoints: `GET /api/v1/state`, `GET /api/v1/<issue>`, `POST /api/v1/refresh`
  - JSON schema mirroring symphony's
  - Loopback-only default bind, configurable
  - HTML / ANSI escaping for any agent-derived strings (symphony hardened this in PRs #22, #23 -- do it from day one)
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
- API schema parity with symphony so external dashboards can interop without per-tool variants.
- All agent-derived strings must be HTML-escaped + ANSI-stripped before rendering.
- Default bind must be loopback only; document the exposure risk if the user changes it.
- TUI must degrade gracefully on Terminal.app (informational warning about RGB color limits).
