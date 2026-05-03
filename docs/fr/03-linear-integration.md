# FR 03: Linear Integration

> Discovery, admission, and deduplication of Linear tickets. Webhook is the hot path; polling is the fallback. The daemon is read-only.

## Purpose

Let the daemon admit assigned tickets without dropping any, while not touching anyone else's tickets, and while respecting the Linear API rate limit (5,000 req/hr) and Linear's recommendations (avoid aggressive polling). All writes to Linear are confined to the agent-side MCP; the daemon process never performs writes.

## User-visible Behavior

- **On webhook delivery**: verify the HMAC signature with the webhook secret in `[linear]` → normalize into the internal issue model → apply the assignee filter and `admit_states` → admit to the orchestrator if it passes.
- **Invalid signature**: reject with the documented unauthorized status code; do not normalize.
- **Polling fallback**: when the webhook cannot be used, poll for issues whose assignee matches and whose state is in `admit_states`, with a cadence cap no longer than 5 minutes.
- **HTTP 429**: apply exponential backoff and record the backoff window in the structured log.
- **Reassignment**: if a running issue is reassigned to someone else, stop the worker immediately → move to `Cleaning` (no retry), and log "assignment loss".
- **Re-admission**: if a previously ignored issue later satisfies the admission conditions, admit it on the next webhook / poll observation.
- **Refresh nudge**: callers can request an out-of-cycle poll via the HTTP API ([15-http-api](15-http-api.md)) or the TUI through `TrackerRefresh`, but the cadence cap and 429 backoff state are never bypassed.

## Capabilities

- **Webhook receiver**: a single endpoint at the workspace level. HMAC signature verification is mandatory.
- **Normalized issue model**: minimally contains `issue id`, `title`, `description`, `current state`, `label set`, `assignee user id`. Other components of the daemon only see the normalized model.
- **Read-only**: no Linear write is ever issued from the daemon process (Linear writes are owned by the agent + the operator's Linear MCP).
- **Deduplication index**: an in-memory index keyed by Linear issue ID. Each entry holds (current daemon state, last observed Linear state snapshot, in-flight subprocess handle).
- **Duplicate-launch prevention**: while an entry exists in any non-terminal state (`Discovered` / `Queued` / `Judging` / `Active` / `AwaitingCleanup` / `Backoff` / `Cleaning`), no additional judge / worker is launched for the issue. Snapshots are updated in place only.
- **Re-admission rule**: even an entry in a terminal state (`Skipped` / `TerminalFailure`) is cleared and starts a fresh cycle from `Discovered` if a new observation satisfies the admission conditions.
- **Serialization of concurrent observations**: when startup recovery and a runtime webhook observe the same issue at the same time, at most one judge / worker subprocess is in flight per issue.

## Boundaries

- **No Linear writes** at all (owned by the agent-side MCP).
- **Generic team / label / project filters** are out of scope (only assignee + admit_states).
- **Trackers other than Linear** (Jira, etc.) are out of scope.
- The daemon **does not mirror observed Linear states into its own state machine** (Linear states are looked up via the tracker every time).

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Linear API; Scope > In > "Linear-ticket-driven implementation runs ..."
- **Requirements**:
  - `roki-mvp Req 3`: Linear Tracker Integration
  - `roki-mvp Req 13.3`: TrackerRefresh trait (for downstream nudges)
- **Design**:
  - `Tracker Adapter` section of `.kiro/specs/roki-mvp/design.md`
- **Related FR**: 04-state-machine-and-recovery, 12-extension-surface (`TrackerRefresh`)
