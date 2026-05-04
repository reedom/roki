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

> Vetoable hook on `Active → Inactive` (the transition the daemon publishes when orchestrator session A emits `action=stop` with `outcome=success`) that structurally validates `review.md` produced by the `finalize_review` phase subprocess ([18-worker-skill-workflow](18-worker-skill-workflow.md)). On failure with retry budget remaining, the gate returns `Deny+RetryWithContext(payload)` and the daemon feeds a `gate_deny` event back to A, which then nominates `action=run_phase` with `phase=implement` and the payload forwarded as `additional_context` ([19-orchestrator-session](19-orchestrator-session.md)). On retry-budget exhaustion, the daemon routes the issue to `Inactive(reason=review_gate_exhausted)` and emits a `daemon_directive` event of `kind=review_gate_exhausted` so A surfaces the failure to Linear via Linear MCP before its terminal exit. The daemon never launches a separate review-only `claude` subprocess.

## Purpose

Trusting only A's self-assessment of "done" lets a ticket reach a PR-ready state without satisfying the EARS acceptance criteria. This gate provides a checkpoint that structurally guarantees "a structurally-validated review exists at the moment A asks to stop". Validation is structural only (file presence / schema / per-criterion status / code-reference reachability) — no LLM is used by the daemon.

The review artifact itself is produced by the `finalize_review` phase subprocess (a daemon-internal synthesis prompt, no kiro skill — see [18-worker-skill-workflow](18-worker-skill-workflow.md)) which A nominates after `validate` and `open_pr` succeed and before A emits `action=stop`. The phase synthesizes `review.md` from the verdicts the kiro skill set accumulated during prior phases (per-task `kiro-review` approvals inside `kiro-impl`, `kiro-validate-impl` GO, `kiro-verify-completion` evidence). The gate validates that artifact at the moment of the `Active → Inactive` transition. The daemon does not launch a separate constrained review turn.

## User-visible Behavior

### Gating flow

1. **Subscription**: at daemon startup, register a subscriber for the vetoable `Active → Inactive` transition ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)).
2. **Trigger**: A emits `action=stop` with `outcome=success`. The daemon publishes the `Active → Inactive` event ([19-orchestrator-session §Response schema](19-orchestrator-session.md), [04-state-machine-and-recovery](04-state-machine-and-recovery.md)).
3. **Validation**: the gate inspects `review.md` at its stable path (see below).
4. **Decision**:
   - **Pass** → `Allow` → daemon transitions the issue to `Inactive(reason=awaiting_linear)`.
   - **Fail (retry remaining)** → `Deny+RetryWithContext(payload)` → daemon keeps A alive, transitions the issue back to `Active`, and feeds A a `gate_deny` event whose `additional_context` is the payload verbatim. A returns `action=run_phase` with `phase=implement` and forwards the payload to the `implement` phase subprocess via the engine adapter's `additional_context` channel (per [19-orchestrator-session §Event catalog](19-orchestrator-session.md), [12-extension-surface](12-extension-surface.md), Req 13.4). The payload is the failing per-criterion entries (criterion id, fail reason, diagnostic text).
   - **Fail (cap exhausted)** → `Deny` → daemon transitions the issue to `Inactive(reason=review_gate_exhausted)` and emits a `daemon_directive` event of `kind=review_gate_exhausted` to A. A writes the matching Linear label + comment via Linear MCP and returns `action=linear_update_done`; the daemon then gracefully terminates A ([14-operator-notifications](14-operator-notifications.md), [19-orchestrator-session](19-orchestrator-session.md)).

### Artifact path and schema

The exact path and required fields of `review.md` are in [`docs/reference/artifacts.md`](../reference/artifacts.md).
Highlights:

- **Path**: `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` (stable; never changed across versions).
- **Schema (required)**: an overall status (`pass`/`fail`) + per-criterion entries (each numeric requirement ID gets a status) + at least one on-disk reachable code reference for each `pass` entry.
- **Producer**: the `finalize_review` phase subprocess (daemon-internal synthesis prompt, no skill) writes the artifact before clean exit. The daemon does not launch a separate review turn.

### Validation rules (no LLM)

The full list of failure codes lives in the "Required elements of review.md" section of [`docs/reference/artifacts.md`](../reference/artifacts.md) (`fail-missing` / `fail-schema` / `fail-evidence` / `fail-missing-spec`).
Substantive judgment of "does the code actually satisfy the criterion" is the responsibility of the kiro skill set that ran inside the prior phase subprocesses (per-task `kiro-review` inside `kiro-impl` and `kiro-validate-impl`). The daemon performs only structural checks.

### Fix-finding loop

- **Trigger**: failing gate result + attempt counter < `extension.gates.review.max_attempts` (default 3).
- **Behavior**:
  - Return the gate decision as `Deny+RetryWithContext(payload)`.
  - The daemon transitions the issue from `Active → Inactive` back to `Active`, keeps A alive, and feeds A a `gate_deny` event with `additional_context = payload`.
  - A returns `action=run_phase` with `phase=implement` and forwards the payload verbatim in `additional_context`. The daemon launches the next `implement` phase subprocess with the payload available through the per-phase context envelope.
  - Increment the attempt counter for the issue.
  - The payload is the **failing per-criterion entries** (criterion id, fail reason, diagnostic text) — kept distinct from the orchestrator system prompt body, so the next `implement` phase subprocess can read it.
- **Phase budget**: the fix loop consumes A's `max_phases` budget (each `action=run_phase` re-nomination is one unit, per [19-orchestrator-session](19-orchestrator-session.md)). Per-phase work is bounded by each phase subprocess's own `--max-turns`. The gate does not consume a separate budget.
- **Counter reset**: when an entry to `Active` happens via a non-veto path (e.g. operator-driven retry), the attempt counter is reset.

### Spec-missing fast-fail

If `requirements.md` ([08-pre-implementation-gate](08-pre-implementation-gate.md)) is not present at validation time, the gate fails immediately with `fail-missing-spec` and routes to `Inactive(reason=review_gate_exhausted)` (no fix loop — the spec gate should already have ensured presence; missing here means corruption or operator intervention). The daemon then emits the `review_gate_exhausted` `daemon_directive` to A as in the cap-exhausted path.

### Self-diagnosis from the agent

The `kiro_review_status` read-only tool (described in detail in [11-agent-tool-boundary](11-agent-tool-boundary.md)) returns gate state read-only:

- artifact presence flag, latest gate result, current attempt counter, configured `max_attempts`, latest failure reason

This is a **read-only self-diagnostic** for phase subprocesses (e.g. so an `implement` phase can re-read the previous failure reason from the same place the daemon reads it on a fix-finding retry). It is not a gate-bypass mechanism.

## Capabilities

- **Mechanical validation only**: no LLM, no heuristic substring search.
- **Fail-closed**: an internal error in the gate is `Deny` (without retry-with-context), routing to `Inactive(reason=review_gate_exhausted)` with the corresponding `daemon_directive` to A.
- **Stable artifact path**: never changed across spec versions (so downstream consumers can depend on it).
- **Spec-missing fast-fail**: a missing `requirements.md` goes straight to escalation.
- **No daemon-launched review subprocess**: the `finalize_review` phase produces `review.md` before A's `action=stop`; the daemon launches no side `claude` invocation for review.
- **Hot reload**: changes to `extension.gates.review.*` apply from the next attempt; in-flight attempt counters are not retroactively reset.
- **Reuse of the roki-mvp pipeline**: structured logs flow through the roki-mvp tracing + redaction layer ([13-observability-logs](13-observability-logs.md)).

## Boundaries

- **Substantive judgment** (does the code satisfy the criterion) is the agent's responsibility, executed inside the prior `implement` / `validate` phase subprocesses via the kiro skill set ([18-worker-skill-workflow](18-worker-skill-workflow.md)).
- **PR operations / merge orchestration / Linear writes** are out of scope (the gate only vetoes; the `open_pr` phase subprocess handles PR creation; A handles all Linear writes via Linear MCP).
- **Multi-turn review sessions launched by the daemon** are out of scope (the `finalize_review` phase is the only producer; the daemon does not launch a side review turn).
- **Persistent review history** is not maintained (logs only; the artifact is the latest one).
- **Cross-issue review correlation** is out of scope.

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-review-gate`; Boundary Strategy > "kiro gates"
- **Requirements**:
  - `roki-review-gate Req 1` - `Req 8`: subscription / artifact / validation / fix loop / config / status tool / escalation
  - `roki-mvp Req 8.3`, `Req 13.4`: vetoable hook on `Active → Inactive`, `additional_context` channel
  - `roki-mvp Req 5.11`: A's `action=stop` triggers the vetoable transition; gate's `Deny+RetryWithContext` is honored via the `gate_deny` event
- **Design**:
  - `.kiro/specs/roki-review-gate/design.md`
- **Related reference**: [artifacts.md](../reference/artifacts.md) (`review.md` schema), [config.md](../reference/config.md) (`extension.gates.review.*`), [extension-surface.md](../reference/extension-surface.md) (vetoable hook, `additional_context`), [log-events.md](../reference/log-events.md) (gate events)
- **Related FR**: 04-state-machine-and-recovery, 02-configuration, 08-pre-implementation-gate, 11-agent-tool-boundary, 12-extension-surface, 13-observability-logs, 14-operator-notifications, 18-worker-skill-workflow, 19-orchestrator-session
