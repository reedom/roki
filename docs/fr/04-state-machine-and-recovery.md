---
refs:
  id: fr:04-state-machine-and-recovery
  kind: fr
  title: "State Machine and Restart Recovery"
  spec: roki-mvp
  implements:
    - req:roki-mvp:8
    - req:roki-mvp:10
---

# FR 04: State Machine and Restart Recovery

> The per-issue in-memory state machine, and restart recovery without persistence. Vetoable transition hooks are the plug-in seam for downstream specs.

## Purpose

Track "what stage is this ticket in right now" per issue inside the daemon, and book-keep subprocess lifecycle and cleanup. **Persistence is intentionally avoided**; on restart the daemon re-reads Linear and the filesystem to reconstruct state. Vetoable transition hooks let downstream specs (gates / observability / distill) plug in without forking the orchestrator core.

## User-visible Behavior

### Kinds of states

Each Linear issue moves through these daemon-local states:

| State | Meaning |
|---|---|
| `Discovered` | Just observed to satisfy the admission conditions |
| `Queued` | Waiting for judge / worker dispatch |
| `Judging` | Setup judge subprocess running |
| `Active` | Main worker subprocess running |
| `AwaitingCleanup` | Worker exited cleanly; waiting for Linear to reach a terminal state |
| `Backoff` | Worker exited non-cleanly; retry budget remains; waiting for the next attempt |
| `Cleaning` | Tracker observation triggered cleanup; deleting worktree / tempdir |
| `Skipped` | Terminal: judge returned `noop`, or rejected by allowlist validation |
| `TerminalFailure` | Terminal: stall / max-turns / unknown subtype / retry exhausted / filesystem failure |

> The daemon **does not mirror Linear-side workflow states (review / done / etc.)**. Linear states are looked up via the tracker each time.

### Key transition rules

- **Entering `AwaitingCleanup`**: only from `Active`, and only on a **clean successful subprocess exit**.
- **Entering `Cleaning`**: only from `Active` / `AwaitingCleanup` / `Backoff`, and only via a **tracker observation (terminal Linear state or reassignment)**. A subprocess exit alone does not cause this transition.
- **Entering `Skipped`**: only from `Judging`, on judge `noop` or allowlist rejection.

### Vetoable transition hooks (downstream extension point)

Downstream specs can subscribe to declared-vetoable transitions and return `Allow` / `Deny`:

- **`Queued -> Active`** — for the pre-implementation gate (used in [08-pre-implementation-gate](08-pre-implementation-gate.md))
- **`AwaitingReview -> TerminalSuccess`** — for the pre-PR gate (used in [09-pre-pr-gate](09-pre-pr-gate.md))
- **The pre-cleanup hook on terminal success → workspace cleanup** — for post-merge distill (used in [10-post-merge-distill](10-post-merge-distill.md))

A `Deny` blocks the transition and is recorded in the structured log. Even if a subscriber raises an unhandled error, only that subscriber's failure is logged; the other subscribers and the orchestrator keep running.

### Restart recovery

- At startup: enumerate **all session tempdirs** under the platform-appropriate user cache root, and the **worktrees in each allowlisted repo whose branch name matches the issue identifier pattern**.
- Reconcile each discovered issue identifier against Linear (applying the assignee filter).
- Classify each issue into one of: `resume-active` / `orphaned-session` / `orphaned-worktree` / `fresh-queued` / `no-op`.
- **Orphan**: no matching active / assigned-to-me Linear issue → log only, do not delete automatically (operator notification → [14-operator-notifications](14-operator-notifications.md)).
- **Fresh-queued**: assignee matches and state ∈ `admit_states`, but no session/worktree → enqueue a fresh admission cycle starting from `Discovered`.

## Capabilities

- **Publishing transition events**: each transition publishes a structured event with prev state / next state / trigger source / issue identifier (and repo identifier where applicable).
- **Subscription hooks**: other components can observe transition events, and may veto the declared-vetoable ones.
- **Subscriber failure isolation**: an exception in one subscriber does not affect other subscribers or the orchestrator.
- **No persistent storage**: per-issue runtime state is never written to disk (with the exception of session tempdir contents, worktree contents, and the structured log).

## Boundaries

- **Mirroring Linear state** is not done.
- **A persistent DB** is intentionally not maintained (recovery is a re-read of Linear + filesystem).
- **Cross-issue state correlation** is out of scope (each issue is independent).
- **Visualization / debug UI of the state machine** belongs to [13-observability-logs](13-observability-logs.md), [15-http-api](15-http-api.md), and [16-roki-tui](16-roki-tui.md).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Per-issue session tempdir lifecycle ..."; Boundary Strategy > "in-memory orchestrator with no persistent database", "State machine extension points"
- **Requirements**:
  - `roki-mvp Req 8`: Orchestrator State Machine and Extension Points
  - `roki-mvp Req 10`: Restart Recovery Without Persistent Storage
  - `roki-spec-gate Req 1`: Subscription against `Queued -> Active`
  - `roki-review-gate Req 1`: Subscription against `AwaitingReview -> TerminalSuccess`
  - `roki-distill-postmerge Req 1`: Post-terminal phase activation
- **Design**:
  - `Orchestrator State Machine` section of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-bootstrap.md`
- **Related FR**: 12-extension-surface (overview of how each hook is consumed)
