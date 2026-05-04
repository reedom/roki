---
refs:
  id: fr:16-roki-tui
  kind: fr
  title: "roki-tui"
  spec: roki-observability
  implements:
    - req:roki-observability:8
    - req:roki-observability:9
    - req:roki-observability:10
    - req:roki-observability:11
    - req:roki-observability:6.4
    - req:roki-observability:14.4
---

# FR 16: roki-tui

> A ratatui binary shipped as a cargo target independent of the daemon. It calls the HTTP API in a refresh loop and renders active workers / per-issue state / escalations. Has local-only escalation acknowledgement and a manual refresh action.

## Purpose

A single-terminal view of daemon state at second-by-second granularity. The TUI's lifecycle is independent of the daemon. Safe to operate under v1's loopback assumption (no auth, no TLS).

## User-visible Behavior

### Startup and connection

- **`roki-tui <api-url>`**: connects to the given API URL, fetches the initial snapshot via `GET /api/v1/state` ([15-http-api](15-http-api.md)), and renders a live state view within the documented startup window.
- **Refresh loop**: re-fetches `GET /api/v1/state` at the configured cadence. If the API returns non-2xx, an error is shown in the status bar; the TUI does not exit.
- **Quit key**: a clean exit on the documented quit key. Restores the terminal to its original mode.

### Render contents

- Active worker list
- Per-issue current state
- Latest lifecycle event summary for each issue
- Escalation queue
- Aggregate token usage
- Aggregate rate-limit window

### Escalation acknowledgement (local only)

- **Acknowledge key**: pressing it on a selected row sets the acknowledged flag in local UI state.
- **Visual distinction**: distinguished by both color and a non-color glyph (visible even on RGB-restricted environments such as Terminal.app).
- **Daemon is not notified**: this is a fully local UI affordance in v1 (an API extension may be considered in a future spec).
- **Auto clear**: the ack state clears automatically when the corresponding escalation no longer appears in the next API snapshot.

### Refresh action

- **Refresh key**: the documented refresh key fires `POST /api/v1/refresh`, and the response status is shown in the status bar.
- **In-flight indicator**: snapshot updates keep rendering while a refresh request is in flight (non-blocking).
- **Error display**: on failure / non-2xx, the HTTP status + a short error message appears in the status bar; the TUI does not exit.
- **Debounce**: respects the API-side minimum refresh interval; bursts are coalesced into a single in-flight request.

### Terminal compatibility

- **Primary**: iTerm2 / Ghostty / WezTerm / Alacritty on macOS + Linux.
- **Degradation**: when a terminal that does not support 24-bit RGB is detected (e.g. macOS Terminal.app), fall back to the 16/256-color palette and emit a one-time informational notice in the status bar at startup.
- **Features not used**: no Sixel, no Kitty graphics, no advanced mouse-tracking dependencies.
- **State marker glyphs**: only printable ASCII or broadly-supported Unicode (so they do not become missing-glyph on Terminal.app).
- **Windows**: out of scope (running `roki-tui` on Windows produces a not-supported message and a non-zero exit).

### Defense-in-depth sanitization

- Even though the API side has already stripped, `roki-tui` re-applies **ANSI strip + control-character removal** to received strings ([15-http-api](15-http-api.md)).

### Shared types

- Request / response types are imported only from `roki-api-types` (the shared crate). Local redefinition is forbidden.

### Logging

- At TUI startup, emit a structured event to **its own stderr** (not into the daemon log). Fields: the API URL it connected to, the refresh cadence.

## Capabilities

- **Independent binary**: the daemon's up/down state and the TUI's up/down state are unrelated.
- **API client only**: does not embed the daemon; communicates only over HTTP.
- **Shared schema**: server and TUI stay in sync via `roki-api-types`.
- **Defense in depth**: re-strips on the TUI even if something leaks through the server.

## Boundaries

- **Authentication / TLS** are out of scope (v1).
- **Mutating actions from the TUI** are limited to `POST /refresh` (escalation ack is local).
- **Persistent UI state** (the previous selection / acknowledged state, etc.) is out of scope (only inside the current session).
- **Web UI** is out of scope (a future spec).
- **Daemon-side ack persistence** is out of scope (v1).

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-observability`; Constraints > Platform (terminal compatibility)
- **Requirements**:
  - `roki-observability Req 8` - `Req 11`: roki-tui binary / escalation ack / refresh action / terminal compatibility
  - `roki-observability Req 6.4`: defense-in-depth sanitization on the TUI side
  - `roki-observability Req 14.4`: TUI startup logging
- **Design**:
  - `roki-tui` section of `.kiro/specs/roki-observability/design.md`
- **Related FR**: 15-http-api
