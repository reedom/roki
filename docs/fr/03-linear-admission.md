---
refs:
  id: fr:03-linear-admission
  kind: fr
  title: "Linear Admission"
  spec: roki-mvp
  implements:
    - req:roki-mvp:3
  related:
    - fr:02-configuration
    - fr:07-recovery
    - fr:10-http-api
    - fr:01-engine-model
---

# FR 03: Linear Admission

> Discovery and admission of Linear tickets via webhook (hot path) or polling (fallback). Webhook signature verification, normalized event model, assignee + repo allowlist gating, polling cadence cap, and 429 backoff. The daemon is read-only against Linear; the diff cache and rule dispatch live in [07-recovery](07-recovery.md) and [01-engine-model](01-engine-model.md).

## Purpose

Admit assigned tickets without drops, never touch others' tickets, and respect the Linear API rate limit (5,000 req/hr) and Linear's no-aggressive-polling recommendation. Linear writes from inside a phase subprocess go through whatever MCP / CLI the operator's cli line provides; the daemon process itself never writes.

## User-visible Behavior

### Webhook intake

- **HMAC verification**: every webhook is verified against `roki.toml [linear.webhook].secret`. Invalid signature → respond with HTTP `401 Unauthorized` and discard the payload without normalization.
- **Normalization**: a verified payload is normalized into the internal issue model (see Capabilities). The normalized model is the only thing later layers see.
- **Forward to admission**: the normalized event is handed to the admission filter (next section). Admission decides whether to update the diff cache and re-evaluate rules.

### Admission filter

Before any cache update ([07-recovery §Diff cache](07-recovery.md)), the daemon evaluates the admission filter declared in WORKFLOW.toml ([02-configuration §WORKFLOW.toml](02-configuration.md)):

1. **Assignee gate**: `ticket.assignee == [admission].assignee`. The literal `me` resolves to the API token holder. Failure → silent eviction (logged but not surfaced to Linear).
2. **Repo resolution**: `[[admission.repos]]` first-match → resolves the ticket's ghq repo identifier and its per-repo `workflow` path (or fall back to the top-level WORKFLOW.toml entries). No match → silent eviction (`reason: repo_unresolvable`).

Tickets that fail admission are not added to the cache. If the ticket was previously cached and the new webhook fails admission (assignee change, repo matcher no longer hits), the cache entry is evicted; if a cycle is currently in flight for that ticket, the cycle is allowed to terminate naturally and the worktree + session_tempdir are deleted afterward as orphan cleanup ([05-worktree-and-session](05-worktree-and-session.md)).

### Polling fallback

The webhook receiver is mandatory (`[linear.webhook]` is required in `roki.toml`). Polling exists only as a runtime fallback for transient webhook outages — Linear cloud unreachable, network partition, the webhook receiver port becoming temporarily unbindable. When the daemon detects such an outage, it polls Linear for issues satisfying the assignee filter, optionally narrowed by status:

- If **every** `[[rule]]` and `[[cleanup]]` entry across WORKFLOW.toml plus every per-repo TOML declares an explicit `when.status`, the union of those values becomes a Linear-side status filter (small, bounded query).
- If **any** entry omits `when.status` (i.e. matches any state), the status filter is dropped and the query enumerates every ticket the assignee owns. The daemon emits an info log at startup naming the entry that triggered the drop, so operators concerned about Linear API budget can add an explicit `when.status` and shrink the query.

Cadence is governed by `roki.toml [linear].polling.cadence_seconds` (default `300`, validation minimum `60`). The cap is enforced even when a refresh nudge arrives (see below). Polling stops automatically once webhook delivery resumes (Linear delivers a fresh webhook the daemon successfully verifies).

### Rate limit

- **HTTP 429**: apply exponential backoff and record the backoff window in the structured log. Backoff state persists across both webhook-driven and polling-driven calls.
- **Cadence cap**: nothing inside the daemon (refresh nudges, escalation handlers, recovery enumeration) bypasses the polling cadence cap or the 429 backoff window.

### Diff observation

The diff cache decides what counts as a change ([07-recovery §Diff cache](07-recovery.md)). For the Linear-side surface, the daemon tracks `(status, labels, assignee)` per ticket. Webhook events that announce changes outside that triple (description edits, comments, reactions) update Linear's own state but do not start a cycle.

`[[admission.repos]]` resolution (including its `title` / `body` matchers) runs **once per cache entry**, at first admission. The resolved `repo` and `workflow_path` are sticky for the lifetime of the cache entry. Subsequent title / body / label / assignee changes do not re-resolve admission.repos. Operators that need a ticket reassigned to a different repo evict the entry first (close-and-reopen the Linear ticket, or revoke-and-restore the assignee) so the next admission picks the new match.

### Reassignment

When the assignee on a previously admitted ticket changes to someone other than `[admission].assignee`:

1. The diff cache evicts the entry.
2. If a cycle is currently in flight, it runs to natural end (queue mode); afterward the daemon deletes the worktree + session_tempdir as orphan cleanup.
3. No Linear write is performed by the daemon. Operators that want a Linear comment on reassignment author a `[[cleanup]]` entry whose run phase performs the write.

There is no separate `Cleaning` state in the daemon ([07-recovery](07-recovery.md)).

### Re-admission

An issue that was previously evicted (assignee mismatch, repo unresolvable) and later satisfies the admission filter is re-admitted on the next webhook or poll observation. The re-admission inserts a fresh cache entry and proceeds through normal rule evaluation; no carryover from the prior admission window is preserved.

### Refresh nudge

Operators (TUI, external scripts, observability components) can request an out-of-cycle Linear refresh through the HTTP API ([10-http-api](10-http-api.md)). The nudge bumps the polling cadence forward by one tick subject to the cap and the current 429 backoff. If a nudge arrives during a backoff window it is dropped (logged) rather than queued.

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
- **Related FR**: [02-configuration](02-configuration.md), [07-recovery](07-recovery.md), [10-http-api](10-http-api.md), [01-engine-model](01-engine-model.md).
