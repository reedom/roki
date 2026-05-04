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

> The per-issue in-memory state machine (six states, single `Inactive` reason discriminator), and restart recovery without persistence. Transitions are read-only observable; no transition is vetoable.

## Purpose

Track "what stage is this ticket in right now" per issue inside the daemon, and book-keep subprocess lifecycle and cleanup. **Persistence is intentionally avoided**; on restart the daemon re-reads Linear and the filesystem to reconstruct state. Transitions are observable read-only by external subscribers (e.g. observability) but are not vetoable — structural validation of `requirements.md` and `review.md` is owned by orchestrator session inside its own phase-planning loop ([19-orchestrator-session](19-orchestrator-session.md) §Artifact validation), not by daemon-side gate hooks.

## User-visible Behavior

### States (six, plus a discriminator on `Inactive`)

Each Linear issue moves through these daemon-local states:

| State | Meaning |
|---|---|
| `Pending` | Idle / waiting. Entry state from admission and from re-admission (Req 3.14). Orchestrator session the orchestrator is launched on `Discovered → Pending` per [19-orchestrator-session](19-orchestrator-session.md). |
| `Judging` | Awaiting the orchestrator's `admission_decision` response to the `admission_request` event (per [19-orchestrator-session §Event catalog](19-orchestrator-session.md)). |
| `Active` | Phase subprocess in flight (the phase the orchestrator nominated via `action=run_phase`) |
| `Backoff` | Non-clean phase exit + retry budget remains; awaiting backoff timer expiry, then back to `Active` for the orchestrator's next `run_phase` directive |
| `Inactive` | Not running. Carries a `reason` discriminator (see below); only some reasons are eligible for tracker-driven cleanup |
| `Cleaning` | Tracker-observed terminal Linear state or reassignment triggered cleanup; deleting worktree / tempdir |

> The daemon **does not mirror Linear-side workflow states (review / done / etc.)**. Linear states are looked up via the tracker each time.

### `Inactive.reason` discriminator

`Inactive` is the only "stopped" state. Its `reason` field is structured-log / TUI / cleanup-eligibility metadata, **not** an orchestrator-internal transition input. Possible values:

| reason | When set | Auto-cleanup eligible? |
|---|---|---|
| `noop` | the orchestrator returned `judge=noop` on `admission_request` (per [19-orchestrator-session](19-orchestrator-session.md)) | yes |
| `awaiting_linear` | the orchestrator emitted `action=stop` with `outcome=success` and review gate `Allow`ed the `Active → Inactive` transition | yes |
| `needs_split` | the orchestrator classified the issue as touching more than one allowlisted repo (`judge=needs_split` in `admission_decision`); the orchestrator wrote the matching Linear label + comment in the same turn (per [19-orchestrator-session](19-orchestrator-session.md)) | yes |
| `allowlist_rejected` | the orchestrator's `act` classification named a repo not in the allowlist, or the orchestrator returned `judge=allowlist_rejected`; the orchestrator wrote the matching Linear feedback in the same turn (per [19-orchestrator-session](19-orchestrator-session.md)) | yes |
| `orchestrator_crash` | The orchestrator crashes (SIGSEGV, non-zero exit without a `stop` action, or orchestrator stall (no event in N seconds) — surfaced via TUI escalation queue only ([19-orchestrator-session §Failure modes](19-orchestrator-session.md)) | no — preserve forensics |
| `orchestrator_unparseable` | the orchestrator's response failed JSON schema validation on two consecutive turns (after one daemon-side reprompt) — surfaced via TUI escalation queue only | no — preserve forensics |
| `orchestrator_budget_exhausted` | `extension.orchestrator.max_phases` exhausted while the orchestrator would nominate another phase — surfaced via TUI escalation queue only | no — preserve forensics |
| `stall` | Phase subprocess stalled and was terminated while the orchestrator was no longer alive ([07-worker-execution](07-worker-execution.md)). When the orchestrator is alive, a phase stall is forwarded to the orchestrator as `phase_nonclean` and does not by itself land the issue in `Inactive`. | no |
| `retry_exhausted` | Phase non-clean exit retry budget exhausted; the daemon sent `daemon_directive (kind=retry_exhausted)` and the orchestrator typically follows with `action=stop` (`outcome=failure`) ([07-worker-execution](07-worker-execution.md), `req:roki-mvp:5.10`). Also the catch-all reason for `outcome=failure` from artifact-validation retry-budget exhaustion (the orchestrator surfaced the failure to Linear itself before stopping). | no |
| `fs_poison` | Filesystem error during session/worktree create or remove ([06-worktree-and-session](06-worktree-and-session.md)) | no |
| `orphan` | Restart recovery saw residue with no matching active Linear issue (Req 10.3) | no |

`Auto-cleanup eligible` reasons let `Cleaning` enter when the tracker observes a terminal Linear state or assignment loss. The non-eligible (`failure`-flavored) reasons retain the worktree/session for inspection until the operator manually closes the Linear ticket; only then does cleanup proceed.

### Key transition rules

- **`Discovered → Pending`**: the orchestrator is launched (per [19-orchestrator-session §Lifecycle](19-orchestrator-session.md)).
- **`Pending → Judging`**: the daemon sends an `admission_request` event to the orchestrator and awaits the orchestrator's `admission_decision`.
- **`Judging → Active`**: the orchestrator returned `judge=act` naming exactly one allowlisted repo + worktree created. No subscriber veto — the orchestrator's first `action=run_phase` (typically `materialize_spec`) follows. The prior daemon-side spec-gate veto window is removed; the orchestrator reads `requirements.md` itself after `materialize_spec` clean-exits and decides whether to retry the phase or proceed (per [19-orchestrator-session §Artifact validation](19-orchestrator-session.md)).
- **`Judging → Inactive`**: the orchestrator returned `judge=noop` (`reason=noop`), `judge=needs_split` (`reason=needs_split`), `judge=allowlist_rejected` (`reason=allowlist_rejected`), or the orchestrator's response failed schema validation on two consecutive turns (`reason=orchestrator_unparseable`). For `needs_split` and `allowlist_rejected`, the orchestrator writes the matching Linear feedback in the same turn — there is no separate `judge_unparseable` reason because the orchestrator is the judge.
- **`Active → Inactive`**: only on the orchestrator's `action=stop` directive. No subscriber veto — the orchestrator reads `review.md` itself after `finalize_review` clean-exits and only emits `action=stop` once it has decided (per [19-orchestrator-session §Artifact validation](19-orchestrator-session.md)). On the orchestrator's `outcome=success` → `Inactive(reason=awaiting_linear)`. On `outcome=failure` → `Inactive(reason=retry_exhausted)` (or whichever non-cleanup reason was already attached by a prior `daemon_directive`). On `outcome=cancelled` → routed via the `tracker_terminal` event path. Phase subprocess exit alone does not cause this transition.
- **`Active → Backoff`**: the orchestrator re-nominated the same phase (`action=run_phase`) after a `phase_nonclean` and the retry budget still has room (per [07-worker-execution](07-worker-execution.md)).
- **`Backoff → Active`**: backoff timer expired; the daemon spawns the phase subprocess the orchestrator nominated.
- **`Active → Inactive(reason=stall|retry_exhausted|orchestrator_crash|orchestrator_unparseable|orchestrator_budget_exhausted)`**: phase stall when the orchestrator is dead, ticket-level retry budget exhausted (when the orchestrator subsequently `action=stop`s after the `daemon_directive (kind=retry_exhausted)`), orchestrator crash, schema drift, or budget exhaustion. The three orchestrator-dead reasons are not auto-cleanup eligible and surface via TUI escalation queue only.
- **`Cleaning`**: only entered from `Active` / `Backoff` / `Inactive` (cleanup-eligible reasons), and only via a **tracker observation** (terminal Linear state or reassignment). an orchestrator subprocess exit alone never causes this transition. If the orchestrator or any phase subprocess is still running for the issue at the moment of observation, the daemon terminates them before performing cleanup.

### Read-only transition observers

Subscribers may observe transition events read-only — there are no vetoable transitions. Observability ([15-http-api](15-http-api.md), [16-roki-tui](16-roki-tui.md)) is the primary consumer; structured logs are emitted alongside.

The prior `Judging → Active` (spec gate) and `Active → Inactive` (review gate) vetoable hooks are removed alongside the gates themselves. Structural validation of `requirements.md` and `review.md` is owned by orchestrator session inside its own phase-planning loop ([19-orchestrator-session §Artifact validation](19-orchestrator-session.md)).

Even if a subscriber raises an unhandled error, only that subscriber's failure is logged; the other subscribers and the orchestrator keep running.

### Restart recovery

> Orchestrator session the orchestrator is **not** persisted across daemon restarts. A fresh the orchestrator is launched for each re-admitted issue when the issue re-enters `Pending` (per [19-orchestrator-session §Lifecycle](19-orchestrator-session.md), `req:roki-mvp:8.5`); in-flight A turns and any orchestrator-internal scratch state are discarded.

- At startup: enumerate **all session tempdirs** under the platform-appropriate user cache root, and the **worktrees in each allowlisted repo whose branch name matches the issue identifier pattern**.
- Reconcile each discovered issue identifier against Linear (applying the assignee filter).
- Classify each issue into one of: `resume-active` / `orphaned-session` / `orphaned-worktree` / `fresh-queued` / `no-op`.
  - `resume-active` → `Pending` (re-enters the normal admission flow with a freshly launched orchestrator).
  - `orphaned-session` / `orphaned-worktree` → `Inactive(reason=orphan)`. The daemon does not dispatch a separate subprocess for surfacing; if a future the orchestrator is alive for any in-flight issue the daemon sends `daemon_directive (kind=orphan)` to that the orchestrator so the orchestrator writes the matching Linear feedback ([14-operator-notifications](14-operator-notifications.md)). Otherwise the orphan surfaces via structured log + TUI escalation queue only (Req 10.3).
  - `fresh-queued` → `Pending`.
  - `no-op` → no entry created.

## Capabilities

- **Single state set**: six states + an `Inactive.reason` discriminator. State key is the Linear issue identifier alone (single repo per ticket, per Req 2.6 / Req 4).
- **Publishing transition events**: each transition publishes a structured event with prev state / next state / trigger source / issue identifier / repo identifier where applicable; `Inactive` transitions additionally include the `reason`.
- **Subscription hooks**: other components can observe transition events read-only; no transition is vetoable.
- **Subscriber failure isolation**: an exception in one subscriber does not affect other subscribers or the orchestrator.
- **No persistent storage**: per-issue runtime state is never written to disk (with the exception of session tempdir contents, worktree contents, and the structured log).

## Boundaries

- **Mirroring Linear state** is not done.
- **A persistent DB** is intentionally not maintained (recovery is a re-read of Linear + filesystem).
- **Cross-issue state correlation** is out of scope (each issue is independent).
- **Per-repo state** is out of scope: one ticket = one repo (multi-repo tickets are rejected upstream by the orchestrator's `judge=needs_split` admission decision per [19-orchestrator-session](19-orchestrator-session.md)).
- **Visualization / debug UI of the state machine** belongs to [13-observability-logs](13-observability-logs.md), [15-http-api](15-http-api.md), and [16-roki-tui](16-roki-tui.md).
- **A pre-cleanup vetoable hook between terminal success and workspace deletion** is no longer provided — post-merge distill is handled in CI, not by the daemon.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Per-issue session tempdir lifecycle ..."; Boundary Strategy > "in-memory orchestrator with no persistent database", "State machine extension points"
- **Requirements**:
  - `roki-mvp Req 8`: Orchestrator State Machine and Extension Points
  - `roki-mvp Req 10`: Restart Recovery Without Persistent Storage
- **Design**:
  - `Orchestrator State Machine` section of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-bootstrap.md`
- **Related FR**: [01-daemon-lifecycle](01-daemon-lifecycle.md), [07-worker-execution](07-worker-execution.md), [12-extension-surface](12-extension-surface.md), [14-operator-notifications](14-operator-notifications.md), [19-orchestrator-session](19-orchestrator-session.md)
