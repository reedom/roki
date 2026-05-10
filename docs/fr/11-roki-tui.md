---
refs:
  id: fr:11-roki-tui
  kind: fr
  title: "roki-tui"
  spec: roki-tui-foundation
  related:
    - fr:08-observability-logs
    - fr:06-failure-handling
    - fr:10-http-api
    - fr:09-log-access-cli
---

# FR 11: roki-tui

> A ratatui binary shipped as a cargo target independent of the daemon. Polls the HTTP API and renders the ticket list, ticket detail (cycle history + log tail), the live event stream, and the escalation queue. Local-only escalation acknowledgement and a manual refresh action.

## Purpose

A single-terminal view of daemon state at second-by-second granularity. The TUI's lifecycle is independent of the daemon. Safe to operate under v1's loopback assumption (no auth, no TLS).

## User-visible Behavior

### Startup and connection

- **`roki-tui <api-url>`**: connects to the given API URL, fetches the initial ticket snapshot via `GET /api/tickets` ([10-http-api](10-http-api.md)), and renders the initial live view promptly after connection.
- **Polling loop**: re-fetches `/api/tickets`, `/api/events?since=<latest_seq>`, and `/api/escalations` at the cadences configured in the TUI config file (see §Configuration). If the API returns non-2xx, an error is shown in the status bar; the TUI does not exit.
- **Quit key**: a clean exit on the documented quit key. Restores the terminal to its original mode.

### Configuration

`roki-tui` reads `~/.config/roki-tui/config.toml` at startup. The file is optional; absent values fall back to documented defaults.

```toml
[polling]
tickets_seconds = 2        # default 2,  validation min 1
events_seconds = 1         # default 1,  validation min 1
escalations_seconds = 5    # default 5,  validation min 1
```

CLI flags (`roki-tui --tickets-cadence ... --events-cadence ... --escalations-cadence ...`) override config-file values. Validation failure refuses startup with an error written to stderr.

### Views

The TUI renders four views; the operator switches between them with documented keys:

- **Tickets**: list of admitted tickets (id, repo, status, labels, assignee, in-flight cycle id, last event timestamp). Sortable by last event time.
- **Ticket detail**: cycle history for the selected ticket (cycle id, kind, trigger, started_at, ended_at, terminal id, total visits), plus a tail view that streams the most recent visit's stdout via `GET /api/tickets/{id}/cycles/{cycle_id}/visits/{n}/{state_id}/stdout` (`{state_id}` defaults to the cycle's last-spawned state).
- **Events**: cross-ticket structured event stream (live tail of the event ring buffer). Filterable by kind, ticket, cycle.
- **Escalations**: outstanding escalation queue entries (kind, state_id, ticket id, cycle id, error text). Local-only acknowledgement clears the visual highlight without notifying the daemon.

There is no daemon-side state-machine view: every ticket is either `idle` or `cycling`, and the in-flight cycle id (when present) is the live indicator.

### Escalation acknowledgement (local only)

- **Acknowledge key**: pressing it on a selected escalation row sets the acknowledged flag in local UI state.
- **Visual distinction**: distinguished by both color and a non-color glyph (visible even on RGB-restricted environments such as Terminal.app).
- **Daemon is not notified**: this is a fully local UI affordance in v1 (an API extension may be considered in a future spec).
- **Auto clear**: the ack state clears automatically when the corresponding escalation no longer appears in the next API snapshot.

### Refresh action

- **Refresh key**: the documented refresh key fires `POST /api/refresh`, and the response status is shown in the status bar.
- **In-flight indicator**: snapshot updates keep rendering while a refresh request is in flight (non-blocking).
- **Error display**: on failure / non-2xx, the HTTP status + a short error message appears in the status bar; the TUI does not exit.
- **Debounce**: respects the API-side minimum refresh interval; bursts are coalesced into a single in-flight request.

### Log inspection

The ticket-detail view exposes "open in `roki log`" shortcuts that print the appropriate `roki log --ticket <id> --cycle <uuid> --iter <n> --state <state_id> --stream <stream>` command line for the operator to copy. The TUI does not embed the full log content beyond the latest-visit-stdout tail in the detail view; operators inspecting full captures use `roki log` outside the TUI.

### Terminal compatibility

- **Primary**: iTerm2 / Ghostty / WezTerm / Alacritty on macOS + Linux.
- **Degradation**: when a terminal that does not support 24-bit RGB is detected (e.g. macOS Terminal.app), fall back to the 16/256-color palette and emit a one-time informational notice in the status bar at startup.
- **Features not used**: no Sixel, no Kitty graphics, no advanced mouse-tracking dependencies.
- **State marker glyphs**: only printable ASCII or broadly-supported Unicode (so they do not become missing-glyph on Terminal.app).
- **Windows**: out of scope (running `roki-tui` on Windows produces a not-supported message and a non-zero exit).

### Defense-in-depth sanitization

- Even though the API side has already stripped, `roki-tui` re-applies **ANSI strip + control-character removal** to received strings ([10-http-api](10-http-api.md)).

### Shared types

- Request / response types are imported only from `roki-api-types` (the shared crate). Local redefinition is forbidden.

### Logging

- At TUI startup, emit a structured event to **its own stderr** (not into the daemon log). Fields: the API URL it connected to, the resolved `[polling]` cadences, and the chosen color palette.

## Capabilities

- **Independent binary**: the daemon's up/down state and the TUI's up/down state are unrelated.
- **API client only**: does not embed the daemon; communicates only over HTTP.
- **Four-view layout**: tickets, ticket detail, events, escalations.
- **Shared schema**: server and TUI stay in sync via `roki-api-types`.
- **Defense in depth**: re-strips on the TUI even if something leaks through the server.

## Boundaries

- **Authentication / TLS** are out of scope (v1).
- **Mutating actions from the TUI** are limited to `POST /refresh` (escalation ack is local).
- **Persistent UI state** (the previous selection / acknowledged state, etc.) is out of scope (only inside the current session).
- **Web UI** is out of scope (a future spec).
- **Daemon-side ack persistence** is out of scope (v1).
- **State-transition diagram view** is out of scope: the daemon has only `idle` / `cycling`, and the cycle id is the live indicator. There is nothing to render as a transition graph.

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-observability`; Constraints > Platform (terminal compatibility).
- **Requirements**:
  - `roki-observability Req 8` – `Req 11`: roki-tui binary / escalation ack / refresh action / terminal compatibility.
  - `roki-observability Req 6.4`: defense-in-depth sanitization on the TUI side.
  - `roki-observability Req 14.4`: TUI startup logging.
- **Related FR**: [08-observability-logs](08-observability-logs.md), [06-failure-handling](06-failure-handling.md), [10-http-api](10-http-api.md), [09-log-access-cli](09-log-access-cli.md).
