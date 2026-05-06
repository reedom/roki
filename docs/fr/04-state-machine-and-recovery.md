---
refs:
  id: fr:04-state-machine-and-recovery
  kind: fr
  title: "Diff Cache and Recovery"
  spec: roki-mvp
  implements:
    - req:roki-mvp:8
    - req:roki-mvp:10
  related:
    - fr:01-daemon-lifecycle
    - fr:02-configuration
    - fr:03-linear-integration
    - fr:06-worktree-and-session
    - fr:14-operator-notifications
    - fr:20-rule-and-cycle-engine
---

# FR 04: Diff Cache and Recovery

> The per-ticket in-memory diff cache the daemon keeps to detect Linear status / labels / assignee changes, plus the cold-start / restart-recovery flow that rebuilds it from Linear and disk on every daemon launch. The daemon does not track execution stages itself — execution is bounded by the cycle ([20-rule-and-cycle-engine](20-rule-and-cycle-engine.md)).

## Purpose

Workflow stages live entirely inside operator-authored cycles ([20-rule-and-cycle-engine](20-rule-and-cycle-engine.md)). The daemon's per-ticket bookkeeping is small:

- For each admitted ticket: the most recent `(status, labels, assignee)` triple, the resolved repo, the resolved per-repo workflow path, and a flag for "cycle in flight". That is the diff cache.
- A queue of pending re-evaluations (when a webhook arrives mid-cycle, the cache updates immediately but rule re-evaluation defers until the cycle ends).

This file documents the diff cache and the cold-start / restart-recovery procedure that builds it from scratch each time the daemon process starts.

## User-visible Behavior

### Admission filter

Before any cache update, the daemon evaluates the admission filter ([02-configuration §WORKFLOW.toml](02-configuration.md)):

1. `ticket.assignee == [admission].assignee` (with `me` resolving to the API token holder). Failure → silent eviction (logged but not surfaced to Linear).
2. `[[admission.repos]]` first-match → resolves the ticket's ghq repo and the per-repo `workflow` path (or fall back to the top-level WORKFLOW.toml). No match → silent eviction (`reason: repo_unresolvable`).

Tickets that fail admission are not added to the cache. If the ticket was previously cached and the new webhook fails admission (assignee change, repo matcher no longer hits), the cache entry is evicted; if a cycle is currently in flight for that ticket, the cycle is allowed to terminate naturally and the worktree + session_tempdir are deleted afterward as orphan cleanup.

### Diff cache

Cache key: Linear issue identifier. Cache value:

| Field | Source |
|---|---|
| `repo` | Admission match |
| `workflow_path` | Admission match (per-repo TOML or top-level) |
| `status` | Last seen Linear state |
| `labels` | Last seen Linear label set |
| `assignee` | Last seen Linear assignee |
| `cycle_id` (optional) | Currently in-flight cycle UUID, if any |
| `pending_recheck` (bool) | Set to true when a webhook arrives mid-cycle; cleared after re-evaluation when the cycle ends |
| `last_event_at` | Timestamp |

#### Diff detection

A new webhook for a cached ticket triggers re-evaluation only when at least one of `(status, labels, assignee)` differs from the cached value. Webhooks that announce changes outside this set (description edits, comment events, reactions, etc.) update Linear's own state but do not start a cycle.

#### Cycle dispatch

When a diff is detected and no cycle is in flight, the daemon evaluates lists in priority order ([20-rule-and-cycle-engine §Cycle kinds](20-rule-and-cycle-engine.md)): `[[cleanup]]` first-match, then `[[rule]]` first-match. The first matching entry starts a cycle. If a cycle is already in flight for the ticket, the daemon sets `pending_recheck = true` instead of starting a new one (queue-mode preemption).

#### Cycle in-flight semantics

Only one cycle is in flight per ticket at a time. The cache's `cycle_id` field names it. When the cycle terminates:

1. The daemon clears `cycle_id`.
2. If the cycle was a cleanup cycle, the daemon deletes worktree + session_tempdir and evicts the cache entry.
3. Otherwise, if `pending_recheck` is true, the daemon re-evaluates the lists against the latest `(status, labels, assignee)`. The `pending_recheck` flag is cleared whether or not a new cycle starts.

#### Subscribers

Other components (HTTP API, TUI, structured event log) observe cache changes via the structured event stream ([13-observability-logs](13-observability-logs.md)) — `cycle_started`, `cycle_completed`, `cycle_aborted`, `worktree_created`, `worktree_deleted`, `cold_start_began`, `cold_start_completed`. There is no in-process subscriber API for cache transitions; consumers subscribe to events through the public observability surface.

### Cold start and restart recovery

On every daemon process start (cold or post-crash), the same flow runs and is the only path that re-populates the cache:

1. Load `roki.toml` and `WORKFLOW.toml` (and any per-repo TOMLs referenced through `[[admission.repos]] workflow`). Validate. Refuse to start on validation failure.
2. Query Linear API for tickets satisfying the admission filter (assignee match plus the union of `when.status` values across all `[[rule]]` and `[[cleanup]]` entries). Pagination is cursor-based; the daemon walks the full result set before continuing.
3. For each ticket: resolve repo via `[[admission.repos]]` first-match. On no match, log `reason: repo_unresolvable` and skip. On match, register a cache entry with the current `(status, labels, assignee, repo, workflow_path)`.
4. After the cache is populated, evaluate `[[cleanup]]` then `[[rule]]` first-match for each ticket. On match, start a cycle with `cycle.trigger = "cold_start"` (env var `ROKI_CYCLE_TRIGGER=cold_start`). Cycles for distinct tickets may run concurrently; same-ticket queue ordering still applies.
5. Reconcile disk residue: enumerate session tempdirs under `[paths].session_root` and worktrees under `[paths].worktree_root`. Anything not corresponding to a Linear-API-hit ticket is treated as an orphan and auto-deleted (worktree + session_tempdir removed; one structured log entry per deletion with `reason: orphan`).

For tickets the daemon was previously running cycles for (e.g. crash-restart): the in-flight cycle is gone (subprocess died with the daemon). The fresh cycle launched in step 4 takes over. Any partial files inside the session tempdir from the previous run remain on disk and are accessible via `roki log --cycle <previous-uuid> ...` for forensics; the new cycle uses a fresh cycle UUID.

The trigger value `cold_start` covers both first-launch and post-crash recovery. A future `restart_recovery` trigger value can be added if operators need to distinguish "we know the daemon crashed" from "this is a fresh start"; MVP collapses both into `cold_start`.

### Stop / shutdown

On orderly shutdown ([01-daemon-lifecycle](01-daemon-lifecycle.md)):

1. The daemon stops accepting webhooks and stops launching new cycles.
2. In-flight cycles are SIGTERMed; their pre/run/post subprocesses receive SIGTERM and the configured shutdown grace window applies.
3. The cache is dropped (it is in-memory only; nothing is persisted).
4. Worktrees and session tempdirs are not deleted at shutdown — recovery will reconcile them at the next start.

## Capabilities

- **Mechanical admission**: assignee + repo allowlist match runs in Rust without any LLM call. Skipped tickets cost zero subprocess.
- **Single triple per ticket**: the cache stores `(status, labels, assignee)` plus repo/workflow path, in-flight cycle id, and a recheck flag. State key is the Linear issue identifier (single repo per ticket; multi-repo tickets are handled by operator-authored end-cycle responses).
- **Diff-driven dispatch**: rule re-evaluation runs only when at least one of the tracked fields differs from the cached value.
- **Queue-mode preemption**: only one cycle per ticket at a time. New webhooks update the cache and re-evaluate after the in-flight cycle ends.
- **No persistent storage**: the cache is rebuilt every start from Linear API and disk reconciliation.
- **Cold-start = restart-recovery**: a single procedure handles both. Cycle `trigger` is `cold_start` for both; future trigger values can extend the enum.
- **Orphan auto-delete**: residue not matched by the Linear API result is auto-deleted with one log line per deletion.

## Boundaries

- **No daemon-side execution-stage enum**: execution stages live inside operator-authored cycles, not as daemon-tracked states.
- **No `Inactive.reason` discriminator**: stop-condition distinctions are operator-authored `outcome` strings on terminal post directives.
- **No daemon-driven Linear feedback for skipped tickets**: silent eviction stays silent. Operators that want a Linear comment on a skip (e.g. "this ticket was not assigned to a configured operator") cannot rely on the daemon — the daemon does not have any Linear write path.
- **No mid-cycle preemption of an in-flight cycle by tracker-terminal observations**: a Linear status change to `Done` or `Cancelled` updates the cache; the in-flight cycle runs to natural end; only after it terminates does the cleanup/rule re-evaluation happen. Operators that want forced termination author a `[[cleanup]]` entry whose run phase issues whatever termination signal they want.
- **No mirroring of Linear-side workflow states** is done. Linear states are looked up via the tracker each time.
- **A persistent DB** is intentionally not maintained.
- **Cross-issue state correlation** is out of scope (each issue is independent).
- **Per-repo state** is out of scope: one ticket = one repo. Multi-repo tickets are resolved to the first matching `[[admission.repos]]` entry; operators that detect the ticket spans repos can `directive: "end"` with whatever Linear write they choose to make.
- **Visualization / debug UI of the cache** belongs to [13-observability-logs](13-observability-logs.md), [15-http-api](15-http-api.md), and [16-roki-tui](16-roki-tui.md).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Per-issue session tempdir lifecycle ..."; Boundary Strategy > "in-memory orchestrator with no persistent database".
- **Requirements**:
  - `roki-mvp Req 8`: Orchestrator State Machine and Extension Points (the requirement remains; the implementation collapses to the diff cache plus the cycle engine in [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md)).
  - `roki-mvp Req 10`: Restart Recovery Without Persistent Storage.
- **Design**:
  - `Diff Cache` / `Cold Start` sections of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
- **Related FR**: [01-daemon-lifecycle](01-daemon-lifecycle.md), [02-configuration](02-configuration.md), [03-linear-integration](03-linear-integration.md), [06-worktree-and-session](06-worktree-and-session.md), [14-operator-notifications](14-operator-notifications.md), [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md).
