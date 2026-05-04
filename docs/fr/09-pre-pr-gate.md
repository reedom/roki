---
refs:
  id: fr:09-pre-pr-gate
  kind: fr
  title: "Pre-PR Gate"
  spec: roki-review-gate
  implements:
    - req:roki-review-gate:1
    - req:roki-review-gate:2
    - req:roki-review-gate:3
    - req:roki-review-gate:4
    - req:roki-review-gate:5
    - req:roki-review-gate:6
    - req:roki-review-gate:7
    - req:roki-review-gate:8
---

# FR 09: Pre-PR Gate

> Vetoable hook on `Active → Inactive` (the worker's clean-exit transition) that structurally validates `review.md` produced inside the worker session by the kiro skill set ([18-worker-skill-workflow](18-worker-skill-workflow.md)). On failure, drive a fix-finding loop that re-launches the worker with findings injected via `additional_context`. The daemon does not launch a separate constrained review turn.

## Purpose

Trusting only the agent's self-assessment of "done" lets a ticket reach a PR-ready state without satisfying the EARS acceptance criteria. This gate provides a checkpoint that structurally guarantees "a structurally-validated review exists after implementation". Validation is structural only (file presence / schema / per-criterion status / code-reference reachability) — no LLM is used by the daemon.

The review artifact itself is produced **by the worker session**, not by a separate daemon-launched constrained turn: the worker is responsible for writing `review.md` before clean-exit, synthesized from the verdicts the kiro skill set accumulated during implementation (`kiro-impl` per-task `kiro-review` approvals + `kiro-validate-impl` GO + `kiro-verify-completion` evidence; see [18-worker-skill-workflow](18-worker-skill-workflow.md)). The gate simply validates the artifact at the moment of `Active → Inactive` transition. On failure, the gate Denies the transition and drives a fix-finding loop by re-launching the worker with findings injected via the engine adapter's `additional_context` channel ([12-extension-surface](12-extension-surface.md), Req 13.4).

This keeps the "single bounded `claude` invocation per ticket" principle (the daemon never launches a side review subprocess) while still enforcing review structurally on the daemon side.

## User-visible Behavior

### Gating flow

1. **Subscription**: at daemon startup, register a subscriber for the vetoable `Active → Inactive` transition ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)).
2. **Trigger**: the orchestrator publishes the `Active → Inactive` event when the worker subprocess clean-exits with `subtype: success` ([07-worker-execution](07-worker-execution.md)).
3. **Validation**: the gate inspects `review.md` at its stable path (see below).
4. **Decision**:
   - **Pass** → `Allow` → orchestrator transitions to `Inactive(reason=awaiting_linear)`.
   - **Fail (retry remaining)** → `Deny+RetryWithContext(payload)` → orchestrator transitions back to `Active` and re-launches the worker with `payload` forwarded as `additional_context`. The payload is the failing per-criterion entries (criterion id, fail reason, the diagnostic text) so the worker has the findings on its next turn.
   - **Fail (cap exhausted)** → `Deny` → orchestrator transitions to `Inactive(reason=review_gate_exhausted)` + linear-updater dispatch with a `review_gate_exhausted` directive ([14-operator-notifications](14-operator-notifications.md)).

### Artifact path and schema

The exact path and required fields of `review.md` are in [`docs/reference/artifacts.md`](../reference/artifacts.md).
Highlights:

- **Path**: `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` (stable; never changed across versions).
- **Schema (required)**: an overall status (`pass`/`fail`) + per-criterion entries (each numeric requirement ID gets a status) + at least one on-disk reachable code reference for each `pass` entry.
- **Producer**: the worker session synthesizes it from the kiro skill set's accumulated verdicts ([18-worker-skill-workflow](18-worker-skill-workflow.md), Phase 6). The daemon does not launch a separate review turn.

### Validation rules (no LLM)

The full list of failure codes lives in the "Required elements of review.md" section of [`docs/reference/artifacts.md`](../reference/artifacts.md) (`fail-missing` / `fail-schema` / `fail-evidence` / `fail-missing-spec`).
Substantive judgment of "does the code actually satisfy the criterion" is the responsibility of the kiro skill set that ran inside the worker (per-task `kiro-review` plus `kiro-validate-impl`). The daemon performs only structural checks.

### Fix-finding loop

- **Trigger**: failing gate result + attempt counter < `extension.gates.review.max_attempts` (default 3).
- **Behavior**:
  - Return the gate decision as `Deny+RetryWithContext(payload)` to the orchestrator.
  - Orchestrator transitions the issue from `Active → Inactive` back to `Active`, re-launching a fresh worker subprocess with `payload` forwarded via the engine adapter's `additional_context` channel.
  - Increment the attempt counter for the issue.
  - The payload is the **failing per-criterion entries** (criterion id, fail reason, diagnostic text) — kept distinct from the worker template body, so the worker on the next turn can read it.
- **Worker turn budget**: the fix loop honors the per-worker `max_turns` budget owned by roki-mvp (the gate does not consume a separate budget).
- **Counter reset**: when an entry to `Active` happens via a non-veto path (e.g. operator-driven retry), the attempt counter is reset.

### Spec-missing fast-fail

If `requirements.md` ([08-pre-implementation-gate](08-pre-implementation-gate.md)) is not present at validation time, the gate fails immediately with `fail-missing-spec` and routes to `Inactive(reason=review_gate_exhausted)` (no fix loop — the spec gate should already have ensured presence; missing here means corruption or operator intervention).

### Self-diagnosis from the agent

The `kiro_review_status` agent tool (described in detail in [11-agent-tool-boundary](11-agent-tool-boundary.md)) returns gate state read-only:

- artifact presence flag, latest gate result, current attempt counter, configured `max_attempts`, latest failure reason

This is a **read-only self-diagnostic** for the agent (e.g. so the worker can re-read the previous failure reason from the same place the daemon reads it on a fix-finding retry). It is not a gate-bypass mechanism.

## Capabilities

- **Mechanical validation only**: no LLM, no heuristic substring search.
- **Fail-closed**: an internal error in the gate is `Deny` (without retry-with-context), routing to `Inactive(reason=review_gate_exhausted)`.
- **Stable artifact path**: never changed across spec versions (so downstream consumers can depend on it).
- **Spec-missing fast-fail**: a missing `requirements.md` goes straight to escalation.
- **No daemon-launched review subprocess**: the worker session produces `review.md` before clean-exit; the daemon launches no side `claude` invocation for review.
- **Hot reload**: changes to `extension.gates.review.*` apply from the next attempt; in-flight attempt counters are not retroactively reset.
- **Reuse of the roki-mvp pipeline**: structured logs flow through the roki-mvp tracing + redaction layer ([13-observability-logs](13-observability-logs.md)).

## Boundaries

- **Substantive judgment** (does the code satisfy the criterion) is the agent's responsibility (kiro skill set, [18-worker-skill-workflow](18-worker-skill-workflow.md)).
- **PR operations / merge orchestration / Linear writes** are out of scope (the gate only vetoes; the worker handles PRs / Linear via its own MCP path).
- **Multi-turn review sessions launched by the daemon** are out of scope (the worker is the only producer; the daemon does not launch a side review turn).
- **Persistent review history** is not maintained (logs only; the artifact is the latest one).
- **Cross-issue review correlation** is out of scope.

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-review-gate`; Boundary Strategy > "kiro gates"
- **Requirements**:
  - `roki-review-gate Req 1` - `Req 8`: subscription / artifact / validation / fix loop / config / status tool / escalation
  - `roki-mvp Req 8.3`, `Req 13.4`: vetoable hook on `Active → Inactive`, `additional_context` channel
- **Design**:
  - `.kiro/specs/roki-review-gate/design.md`
- **Related reference**: [artifacts.md](../reference/artifacts.md) (`review.md` schema), [config.md](../reference/config.md) (`extension.gates.review.*`), [extension-surface.md](../reference/extension-surface.md) (vetoable hook, `additional_context`), [log-events.md](../reference/log-events.md) (gate events)
- **Related FR**: 04-state-machine-and-recovery, 02-configuration, 08-pre-implementation-gate, 11-agent-tool-boundary, 12-extension-surface, 13-observability-logs, 14-operator-notifications, 18-worker-skill-workflow
