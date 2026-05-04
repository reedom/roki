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

> The per-issue in-memory state machine (five states, single `Inactive` reason discriminator), the daemon-side mechanical pre-admission-judge that gates state entry, and restart recovery without persistence. Transitions are read-only observable; no transition is vetoable.

## Purpose

Track per-issue stage inside the daemon and book-keep subprocess lifecycle and cleanup. **Persistence is intentionally avoided**; on restart the daemon re-reads Linear and the filesystem to reconstruct state. Transitions are observable read-only; not vetoable — substantive admission classification (Path A/B/C/D/E) runs inside the `classify` phase subprocess ([18-worker-skill-workflow](18-worker-skill-workflow.md)), not in daemon-side gate hooks. Structural validation of `review.md` and (in SPEC_DRIVEN mode) the target spec docs is owned by the orchestrator session ([19-orchestrator-session §Artifact validation](19-orchestrator-session.md)).

## User-visible Behavior

### Pre-admission judge (daemon-side, mechanical, no LLM)

Before any state entry, the daemon evaluates each Linear webhook against four mechanical conditions in order. The judge does not write Linear, does not launch the orchestrator, and does not record state for skipped tickets — only a structured log line.

| # | Condition | On match |
|---|---|---|
| 1 | `ticket.assignee == roki.toml#linear.assignee` | continue |
| 1 | `ticket.assignee != roki.toml#linear.assignee` | **skip** (log only) |
| 2 | `linear_state ∈ roki.toml#linear.admit_states` | continue |
| 2 | `linear_state ∉ roki.toml#linear.admit_states` | **skip** (log only) |
| 3 | `labels ⊇ {roki:ready}` | continue |
| 3 | `labels ⊉ {roki:ready}` | **skip** (log only) |
| 4a | `labels ⊇ {roki:ready, roki:impl}` | enter `Pending` with `mode=SPEC_DRIVEN` |
| 4b | `labels ⊇ {roki:ready}, roki:impl ∉ labels` | enter `Pending` with `mode=NEEDS_CLASSIFY` |
| 4c | `labels ⊇ {roki:impl}, roki:ready ∉ labels` | **skip** (log only — `roki:impl` alone is not authorized) |

`admit_states` is operator-configured in `roki.toml` ([02-configuration §roki.toml](02-configuration.md)). Typical default: `["Todo"]` — tickets that are still being scoped (`Backlog`) or already in progress / under review (`In Progress` / `In Review` / `Done` / `Cancelled`) are silently skipped. A ticket whose state moves out of an admitted state mid-flight does not affect a running orchestrator (mode mutation mid-flight is out of scope, see §Boundaries); the next webhook re-runs the judge against the new state.

The chosen `mode` (`SPEC_DRIVEN` or `NEEDS_CLASSIFY`) is attached to the per-issue state and rendered into the orchestrator's system prompt at launch ([19-orchestrator-session §Lifecycle](19-orchestrator-session.md)). The mode never changes for the lifetime of the orchestrator session; relabeling mid-flight does not re-route an in-flight ticket. Re-evaluation happens on the next webhook.

### States (five, plus a discriminator on `Inactive`)

Each Linear issue moves through these daemon-local states:

| State | Meaning |
|---|---|
| `Pending` | Orchestrator session alive but no phase subprocess running. Entry state from pre-admission-judge pass and from re-entry between phases (after `Active` clean exit and before the orchestrator's next `run_phase` directive). |
| `Active` | Phase subprocess in flight (the phase the orchestrator nominated via `action=run_phase`). |
| `Backoff` | Non-clean phase exit + retry budget remains; awaiting backoff timer expiry, then back to `Active` for the orchestrator's next `run_phase` directive. |
| `Inactive` | Not running. Carries a `reason` discriminator (see below); only some reasons are eligible for tracker-driven cleanup. |
| `Cleaning` | Tracker-observed terminal Linear state or reassignment triggered cleanup; deleting worktree / tempdir. |

> The daemon **does not mirror Linear-side workflow states (review / done / etc.)**. Linear states are looked up via the tracker each time.
>
> The previous `Judging` state is removed: there is no longer an `admission_request` / `admission_decision` round-trip — admission classification happens inside the `classify` phase subprocess (NEEDS_CLASSIFY mode) or inside the orchestrator's first deliberation turn against the operator-named target spec (SPEC_DRIVEN mode). In both cases the orchestrator deliberates from `Pending` and transitions to `Active` only when it nominates a phase.

### `Inactive.reason` discriminator

`Inactive` is the only "stopped" state. Its `reason` field is structured-log / TUI / cleanup-eligibility metadata, **not** an orchestrator-internal transition input. Possible values:

| reason | When set | Auto-cleanup eligible? |
|---|---|---|
| `awaiting_linear` | The orchestrator emitted `action=stop` with `outcome=success` (PR opened, finalize_review passed). | yes |
| `needs_operator` | NEEDS_CLASSIFY mode + `classify` phase returned Path A / C / D / E. The orchestrator wrote a Linear comment with the recommended next manual command and labels in the same turn before stopping. | no — preserve worktree until operator acts |
| `spec_incomplete` | SPEC_DRIVEN mode + the orchestrator's structural check of `<repo>/.kiro/specs/<target>/{spec.json,requirements.md,design.md,tasks.md}` failed (missing files, `approvals.tasks.approved == false`, or target spec name unresolvable from ticket body). The orchestrator wrote a Linear comment naming the missing artifact and the recommended `/kiro-spec-*` command before stopping. | no — preserve worktree until operator acts |
| `needs_split` | The orchestrator detected the ticket touches more than one allowlisted repo and wrote the matching Linear label + comment in the same turn before stopping. | yes |
| `allowlist_rejected` | The orchestrator detected the ticket's named repo is not in the allowlist and wrote the matching Linear feedback in the same turn before stopping. | yes |
| `orchestrator_crash` | The orchestrator crashes (SIGSEGV, non-zero exit without a `stop` action) or stalls (no event in N seconds) — surfaced via TUI escalation queue only ([19-orchestrator-session §Failure modes](19-orchestrator-session.md)). | no — preserve forensics |
| `orchestrator_unparseable` | The orchestrator's response failed JSON schema validation on two consecutive turns (after one daemon-side reprompt) — surfaced via TUI escalation queue only. | no — preserve forensics |
| `orchestrator_budget_exhausted` | `extension.orchestrator.max_phases` exhausted while the orchestrator would nominate another phase — surfaced via TUI escalation queue only. | no — preserve forensics |
| `stall` | Phase subprocess stalled and was terminated while the orchestrator was no longer alive ([07-worker-execution](07-worker-execution.md)). When the orchestrator is alive, a phase stall is forwarded to the orchestrator as `phase_nonclean` and does not by itself land the issue in `Inactive`. | no |
| `retry_exhausted` | Phase non-clean exit retry budget exhausted; the daemon sent `daemon_directive (kind=retry_exhausted)` and the orchestrator typically follows with `action=stop` (`outcome=failure`) ([07-worker-execution](07-worker-execution.md), `req:roki-mvp:5.10`). Also the catch-all reason for `outcome=failure` from `review.md` validation retry-budget exhaustion (the orchestrator surfaced the failure to Linear itself before stopping). | no |
| `fs_poison` | Filesystem error during session/worktree create or remove ([06-worktree-and-session](06-worktree-and-session.md)). | no |
| `orphan` | Restart recovery saw residue with no matching active Linear issue (Req 10.3). | no |

`Auto-cleanup eligible` reasons let `Cleaning` enter when the tracker observes a terminal Linear state or assignment loss. The non-eligible reasons retain the worktree/session for inspection until the operator manually closes the Linear ticket; only then does cleanup proceed.

### Key transition rules

- **(no entry) → Pending**: the daemon's pre-admission-judge passed (assignee match + `roki:ready` label). The orchestrator session is launched with the chosen `mode` rendered into its system prompt (per [19-orchestrator-session §Lifecycle](19-orchestrator-session.md)).
- **Pending → Active**: the orchestrator returned `action=run_phase` and the daemon spawned the phase subprocess.
- **Active → Pending**: the phase subprocess clean-exited and the daemon emitted `phase_complete` to the orchestrator. The orchestrator deliberates the next directive from `Pending`.
- **Active → Backoff**: the orchestrator re-nominated the same phase (`action=run_phase`) after a `phase_nonclean` and the retry budget still has room (per [07-worker-execution](07-worker-execution.md)).
- **Backoff → Active**: backoff timer expired; the daemon spawns the phase subprocess the orchestrator nominated.
- **Pending → Inactive**: the orchestrator emitted `action=stop`. The `outcome` field selects the `Inactive.reason` per the table above. For `needs_operator`, `spec_incomplete`, `needs_split`, and `allowlist_rejected` the orchestrator already wrote the matching Linear feedback in the same turn before stopping.
- **Active → Inactive(reason=stall)**: phase subprocess stalled and was terminated while the orchestrator was no longer alive ([07-worker-execution](07-worker-execution.md)). When the orchestrator is alive a stall is delivered to it as `phase_nonclean` and the orchestrator decides; the issue does not land in `Inactive` directly.
- **Active → Inactive(reason=orchestrator_crash | orchestrator_unparseable | orchestrator_budget_exhausted)**: the orchestrator died, schema-drifted, or exhausted `max_phases` while a phase was running. The daemon SIGTERMs the in-flight phase and routes through one of the three orchestrator-dead reasons. Surfaced via TUI escalation queue only.
- **Cleaning**: only entered from `Active` / `Backoff` / `Pending` / `Inactive` (cleanup-eligible reasons), and only via a **tracker observation** (terminal Linear state or reassignment). An orchestrator subprocess exit alone never causes this transition. If the orchestrator or any phase subprocess is still running for the issue at the moment of observation, the daemon terminates them before performing cleanup.

### Read-only transition observers

Subscribers observe transition events read-only — no vetoable transitions. Observability ([15-http-api](15-http-api.md), [16-roki-tui](16-roki-tui.md)) is the primary consumer; structured logs emit alongside.

A subscriber's unhandled error is logged in isolation; other subscribers and the orchestrator keep running.

### Restart recovery

> Orchestrator sessions are **not** persisted across daemon restarts. A fresh orchestrator launches per re-admitted issue on `Pending` re-entry ([19-orchestrator-session §Lifecycle](19-orchestrator-session.md), `req:roki-mvp:8.5`); in-flight turns and orchestrator-internal scratch state are discarded. The pre-admission-judge re-runs against the current Linear label set, so a relabeling that occurred while the daemon was down is honored on restart.

- At startup: enumerate **all session tempdirs** under the platform-appropriate user cache root, and the **worktrees in each allowlisted repo whose branch name matches the issue identifier pattern**.
- Reconcile each discovered issue identifier against Linear (applying the assignee + label filter).
- Classify each issue into one of: `resume-active` / `orphaned-session` / `orphaned-worktree` / `fresh-queued` / `no-op`.
  - `resume-active` → `Pending` (re-enters the normal admission flow with a freshly launched orchestrator; mode is recomputed from the current Linear label set).
  - `orphaned-session` / `orphaned-worktree` → `Inactive(reason=orphan)`. The daemon does not dispatch a separate subprocess for surfacing; if a future orchestrator is alive for any in-flight issue the daemon sends `daemon_directive (kind=orphan)` to that orchestrator so the orchestrator writes the matching Linear feedback ([14-operator-notifications](14-operator-notifications.md)). Otherwise the orphan surfaces via structured log + TUI escalation queue only (Req 10.3).
  - `fresh-queued` → `Pending`.
  - `no-op` → no entry created.

## Capabilities

- **Mechanical pre-admission**: assignee + fixed-label gating runs in Rust without any LLM call. Skipped tickets cost zero subprocess.
- **Single state set**: five states + an `Inactive.reason` discriminator. State key is the Linear issue identifier alone (single repo per ticket, per Req 2.6 / Req 4).
- **Mode-tagged orchestrator**: each `Pending` entry carries a `mode` flag (`SPEC_DRIVEN` / `NEEDS_CLASSIFY`) determined at pre-admission and rendered into the orchestrator's system prompt; the mode is immutable for the session.
- **Publishing transition events**: each transition publishes a structured event with prev state / next state / trigger source / issue identifier / repo identifier where applicable; `Inactive` transitions additionally include the `reason`.
- **Subscription hooks**: other components can observe transition events read-only; no transition is vetoable.
- **Subscriber failure isolation**: an exception in one subscriber does not affect other subscribers or the orchestrator.
- **No persistent storage**: per-issue runtime state is never written to disk (with the exception of session tempdir contents, worktree contents, and the structured log).

## Boundaries

- **Skipped tickets are silent**: the pre-admission-judge does not write Linear, does not surface to TUI, and does not enter any state. Operators discover skip via structured log only. Adding a Linear comment for skips (e.g. "this ticket was not assigned to a configured operator") is out of scope.
- **Mode mutation mid-flight** is out of scope: relabeling a ticket while its orchestrator is running does not re-route it. The next webhook re-runs pre-admission and may launch a fresh orchestrator if the prior one has stopped.
- **Mirroring Linear state** is not done.
- **A persistent DB** is intentionally not maintained (recovery is a re-read of Linear + filesystem).
- **Cross-issue state correlation** is out of scope (each issue is independent).
- **Per-repo state** is out of scope: one ticket = one repo (multi-repo tickets are rejected by the orchestrator with `outcome=needs_split` per [19-orchestrator-session](19-orchestrator-session.md)).
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
- **Related FR**: [01-daemon-lifecycle](01-daemon-lifecycle.md), [02-configuration](02-configuration.md), [07-worker-execution](07-worker-execution.md), [12-extension-surface](12-extension-surface.md), [14-operator-notifications](14-operator-notifications.md), [18-worker-skill-workflow](18-worker-skill-workflow.md), [19-orchestrator-session](19-orchestrator-session.md)
