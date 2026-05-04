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

> The per-issue in-memory state machine (six states, single `Inactive` reason discriminator), and restart recovery without persistence. Two vetoable transition hooks are the plug-in seam for downstream gate specs.

## Purpose

Track "what stage is this ticket in right now" per issue inside the daemon, and book-keep subprocess lifecycle and cleanup. **Persistence is intentionally avoided**; on restart the daemon re-reads Linear and the filesystem to reconstruct state. Two vetoable transition hooks let the kiro-spec gate ([08-pre-implementation-gate](08-pre-implementation-gate.md)) and the kiro-review gate ([09-pre-pr-gate](09-pre-pr-gate.md)) plug in without forking the orchestrator core.

## User-visible Behavior

### States (six, plus a discriminator on `Inactive`)

Each Linear issue moves through these daemon-local states:

| State | Meaning |
|---|---|
| `Pending` | Idle / waiting. Entry state from admission and from re-admission (Req 3.14). |
| `Judging` | Setup judge subprocess in flight |
| `Active` | Main worker subprocess in flight |
| `Backoff` | Non-clean exit + retry budget remains; awaiting backoff timer expiry, then back to `Active` |
| `Inactive` | Not running. Carries a `reason` discriminator (see below); only some reasons are eligible for tracker-driven cleanup |
| `Cleaning` | Tracker-observed terminal Linear state or reassignment triggered cleanup; deleting worktree / tempdir |

> The daemon **does not mirror Linear-side workflow states (review / done / etc.)**. Linear states are looked up via the tracker each time.

### `Inactive.reason` discriminator

`Inactive` is the only "stopped" state. Its `reason` field is structured-log / TUI / cleanup-eligibility metadata, **not** an orchestrator-internal transition input. Possible values:

| reason | When set | Auto-cleanup eligible? |
|---|---|---|
| `noop` | Judge returned `action=noop` ([05-setup-judge](05-setup-judge.md)) | yes |
| `awaiting_linear` | Worker clean-exited and review gate `Allow`ed the `Active → Inactive` transition | yes |
| `needs_split` | Judge classified the issue as touching more than one allowlisted repo ([05-setup-judge](05-setup-judge.md)) | yes |
| `allowlist_rejected` | Judge classification names a repo not in the allowlist | yes |
| `judge_unparseable` | Judge failed to produce parseable findings even after retry ([05-setup-judge](05-setup-judge.md)) | no — preserve forensics |
| `stall` | Worker stalled and was terminated ([07-worker-execution](07-worker-execution.md)) | no |
| `max_turns_exhausted` | Worker hit `--max-turns` before clean exit | no |
| `unknown_subtype` | Worker's terminal `result` event reported an uncompiled `subtype` | no |
| `retry_exhausted` | Non-clean exit retry budget exhausted ([07-worker-execution](07-worker-execution.md)) | no |
| `review_gate_exhausted` | Pre-PR review gate Denied the clean exit beyond its retry budget ([09-pre-pr-gate](09-pre-pr-gate.md)) | no |
| `fs_poison` | Filesystem error during session/worktree create or remove ([06-worktree-and-session](06-worktree-and-session.md)) | no |
| `orphan` | Recovery saw residue with no matching active Linear issue (Req 10.3) | no |

`Auto-cleanup eligible` reasons let `Cleaning` enter when the tracker observes a terminal Linear state or assignment loss. The non-eligible (`failure`-flavored) reasons retain the worktree/session for inspection until the operator manually closes the Linear ticket; only then does cleanup proceed.

### Key transition rules

- **`Pending → Judging`**: judge launch.
- **`Judging → Active`**: judge `act` + exactly one allowlisted repo + worktree created. **Vetoable hook for the spec gate** ([08-pre-implementation-gate](08-pre-implementation-gate.md)). On `Allow` → Active. On `Deny` with retry budget remaining → re-Judging (gate retries the spec materialization). On `Deny` with retry exhausted → `Inactive(reason=judge_unparseable)`.
- **`Judging → Inactive`**: judge `noop` (`reason=noop`), allowlist rejection (`reason=allowlist_rejected` or `needs_split`), or judge unparseable after retry (`reason=judge_unparseable`).
- **`Active → Inactive`**: only on a clean successful subprocess exit. **Vetoable hook for the review gate** ([09-pre-pr-gate](09-pre-pr-gate.md)). On `Allow` → `Inactive(reason=awaiting_linear)`. On `Deny+RetryWithContext(payload)` with retry budget remaining → back to `Active` (re-launch worker with `additional_context = payload`). On `Deny` with retry exhausted → `Inactive(reason=review_gate_exhausted)`. Subprocess exit alone does not cause this transition; the gate's decision is part of the transition.
- **`Active → Backoff`**: non-clean subprocess exit with retry budget remaining.
- **`Backoff → Active`**: backoff timer expired.
- **`Active → Inactive(reason=...)`**: stall / max-turns / unknown subtype / retry-exhausted (no review gate involvement on these — the agent never produced a clean exit).
- **`Cleaning`**: only entered from `Active` / `Backoff` / `Inactive` (cleanup-eligible reasons), and only via a **tracker observation** (terminal Linear state or reassignment). A subprocess exit alone never causes this transition. If a worker subprocess is still running for the issue at the moment of observation, the daemon terminates it before performing cleanup.

### Vetoable transition hooks (downstream extension point)

Subscribers may observe and influence exactly two transitions:

| Hook | Consumer | Decision shape |
|---|---|---|
| `Judging → Active` | `roki-spec-gate` ([08-pre-implementation-gate](08-pre-implementation-gate.md)) | `Allow` / `Deny` |
| `Active → Inactive` | `roki-review-gate` ([09-pre-pr-gate](09-pre-pr-gate.md)) | `Allow` / `Deny` / `Deny+RetryWithContext(payload)` |

A `Deny` blocks the transition and is recorded in the structured log. The `Deny+RetryWithContext(payload)` form is unique to the review gate and lets it re-launch the worker with fix-finding payload via the engine adapter's `additional_context` channel ([12-extension-surface](12-extension-surface.md), Req 13.4).

Even if a subscriber raises an unhandled error, only that subscriber's failure is logged; the other subscribers and the orchestrator keep running.

### Restart recovery

- At startup: enumerate **all session tempdirs** under the platform-appropriate user cache root, and the **worktrees in each allowlisted repo whose branch name matches the issue identifier pattern**.
- Reconcile each discovered issue identifier against Linear (applying the assignee filter).
- Classify each issue into one of: `resume-active` / `orphaned-session` / `orphaned-worktree` / `fresh-queued` / `no-op`.
  - `resume-active` → `Pending` (re-enters the normal admission flow).
  - `orphaned-session` / `orphaned-worktree` → `Inactive(reason=orphan)` + linear-updater dispatch (Req 10.3).
  - `fresh-queued` → `Pending`.
  - `no-op` → no entry created.

## Capabilities

- **Single state set**: six states + an `Inactive.reason` discriminator. State key is the Linear issue identifier alone (single repo per ticket, per Req 2.6 / Req 4).
- **Publishing transition events**: each transition publishes a structured event with prev state / next state / trigger source / issue identifier / repo identifier where applicable; `Inactive` transitions additionally include the `reason`.
- **Subscription hooks**: other components can observe transition events, and may veto the two declared-vetoable ones.
- **Subscriber failure isolation**: an exception in one subscriber does not affect other subscribers or the orchestrator.
- **No persistent storage**: per-issue runtime state is never written to disk (with the exception of session tempdir contents, worktree contents, and the structured log).

## Boundaries

- **Mirroring Linear state** is not done.
- **A persistent DB** is intentionally not maintained (recovery is a re-read of Linear + filesystem).
- **Cross-issue state correlation** is out of scope (each issue is independent).
- **Per-repo state** is out of scope: one ticket = one repo (multi-repo tickets are rejected upstream by the setup judge per [05-setup-judge](05-setup-judge.md)).
- **Visualization / debug UI of the state machine** belongs to [13-observability-logs](13-observability-logs.md), [15-http-api](15-http-api.md), and [16-roki-tui](16-roki-tui.md).
- **A pre-cleanup vetoable hook between terminal success and workspace deletion** is no longer provided — post-merge distill is handled in CI, not by the daemon.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Per-issue session tempdir lifecycle ..."; Boundary Strategy > "in-memory orchestrator with no persistent database", "State machine extension points"
- **Requirements**:
  - `roki-mvp Req 8`: Orchestrator State Machine and Extension Points
  - `roki-mvp Req 10`: Restart Recovery Without Persistent Storage
  - `roki-spec-gate Req 1`: Subscription against `Judging → Active`
  - `roki-review-gate Req 1`: Subscription against `Active → Inactive`
- **Design**:
  - `Orchestrator State Machine` section of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-bootstrap.md`
- **Related FR**: 12-extension-surface (overview of how each hook is consumed)
