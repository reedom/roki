---
refs:
  id: fr:03-linear-integration
  kind: fr
  title: "Linear Integration"
  spec: roki-mvp
  implements:
    - req:roki-mvp:3
  related:
    - fr:02-configuration
    - fr:04-state-machine-and-recovery
    - fr:15-http-api
    - fr:20-rule-and-cycle-engine
---

# FR 03: Linear Integration

> Discovery and admission of Linear tickets via webhook (hot path) or polling (fallback). The daemon is read-only against Linear; the admission filter and diff cache live in [04-state-machine-and-recovery](04-state-machine-and-recovery.md), and rule dispatch in [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md). This FR covers the wire-level intake plus the assignee / admission semantics as observed at the Linear surface.

## Purpose

Admit assigned tickets without drops, never touch others' tickets, and respect the Linear API rate limit (5,000 req/hr) and Linear's no-aggressive-polling recommendation. Linear writes from inside a phase subprocess go through whatever MCP / CLI the operator's cli line provides; the daemon process itself never writes.

## User-visible Behavior

### Webhook intake

- **HMAC verification**: every webhook is verified against `roki.toml [linear].webhook_secret`. Invalid signature → respond with the documented unauthorized status code without normalization.
- **Normalization**: a verified payload is normalized into the internal issue model (see Capabilities). The normalized model is the only thing later layers see.
- **Forward to admission**: the normalized event is handed to the admission filter ([04-state-machine-and-recovery §Admission filter](04-state-machine-and-recovery.md)). Admission decides whether to update the diff cache and re-evaluate rules.

### Polling fallback

When the webhook is not usable (operator-disabled, network partition, transient failure), the daemon polls Linear for issues that satisfy the assignee filter and whose state is in the union of `when.status` values across all `[[rule]]` and `[[cleanup]]` entries from WORKFLOW.toml. Cadence is capped at no more than once every five minutes. The cap is enforced even when a refresh nudge arrives (see below).

### Rate limit

- **HTTP 429**: apply exponential backoff and record the backoff window in the structured log. Backoff state persists across both webhook-driven and polling-driven calls.
- **Cadence cap**: nothing inside the daemon (refresh nudges, escalation handlers, recovery enumeration) bypasses the polling cadence cap or the 429 backoff window.

### Diff observation

The diff cache decides what counts as a change ([04-state-machine-and-recovery §Diff cache](04-state-machine-and-recovery.md)). For the Linear-side surface, the daemon tracks `(status, labels, assignee)` per ticket. Webhook events that announce changes outside that triple (description edits, comments, reactions) update Linear's own state but do not start a cycle.

### Reassignment

When the assignee on a previously admitted ticket changes to someone other than `[admission].assignee`:

1. The diff cache evicts the entry.
2. If a cycle is currently in flight, it runs to natural end (queue mode); afterward the daemon deletes the worktree + session_tempdir as orphan cleanup.
3. No Linear write is performed by the daemon. Operators that want a Linear comment on reassignment author a `[[cleanup]]` entry whose run phase performs the write.

The previous "stop the worker immediately → move to `Cleaning` → no retry" semantics are replaced by the queue-mode behavior above. There is no separate `Cleaning` state in the daemon anymore ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)).

### Re-admission

An issue that was previously evicted (assignee mismatch, repo unresolvable) and later satisfies the admission filter is re-admitted on the next webhook or poll observation. The re-admission inserts a fresh cache entry and proceeds through normal rule evaluation; no carryover from the prior admission window is preserved.

### Refresh nudge

Operators (TUI, external scripts, observability components) can request an out-of-cycle Linear refresh through the HTTP API ([15-http-api](15-http-api.md)). The nudge bumps the polling cadence forward by one tick subject to the cap and the current 429 backoff. If a nudge arrives during a backoff window it is dropped (logged) rather than queued.

## Capabilities

- **Webhook receiver**: a single endpoint at the workspace level. HMAC signature verification is mandatory.
- **Normalized issue model**: minimally contains issue id, title, description, current state, label set, assignee user id, repo identifier (resolved by admission). Later layers only see the normalized model.
- **Read-only**: no Linear write is ever issued from the daemon process. Writes are confined to phase subprocesses that operators authorize through their cli line.
- **Polling fallback**: implements the cap + 429 backoff contract above, sharing rate-limit accounting with webhook-driven calls.
- **Refresh nudge**: an out-of-cycle poll request that respects the cap and backoff state.
- **Single-flight**: the diff cache ensures at most one cycle per ticket at a time. Concurrent observations of the same ticket at the same instant are serialized through the cache; the cycle dispatch step runs only once.

## Boundaries

- **No Linear writes from the daemon process** at all. Writes belong to phase subprocesses (operator-controlled cli lines).
- **Generic team / label / project filters** are out of scope. The daemon's Linear-side filter is exactly assignee plus the union of `when.status` values used by WORKFLOW.toml entries.
- **Trackers other than Linear** (Jira, etc.) are out of scope.
- **The daemon does not mirror observed Linear states into a state machine.** Linear states are looked up via the tracker each time and held only as the latest cached triple.
- **Linear comment dedup / threading** are out of scope. If an operator's cycle posts duplicate comments after a daemon restart, the operator's cli line must dedup (e.g. by checking the cold-start trigger).

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Linear API; Scope > In > "Linear-ticket-driven implementation runs ...".
- **Requirements**:
  - `roki-mvp Req 3`: Linear Tracker Integration.
  - `roki-mvp Req 13.3`: TrackerRefresh trait (refresh nudge consumer).
- **Design**:
  - `Tracker Adapter` section of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
- **Related FR**: [02-configuration](02-configuration.md), [04-state-machine-and-recovery](04-state-machine-and-recovery.md), [15-http-api](15-http-api.md), [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md).
