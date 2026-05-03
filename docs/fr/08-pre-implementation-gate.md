---
refs:
  id: fr:08-pre-implementation-gate
  kind: fr
  title: "Pre-Implementation Gate"
  spec: roki-spec-gate
  implements:
    - requirements:roki-spec-gate
---

# FR 08: Pre-Implementation Gate

> Gate `Queued -> Active` with a vetoable hook. Do not let the worker enter `Active` unless `.kiro/specs/<issue>/requirements.md` (in EARS form) exists.

## Purpose

When an agent jumps straight from a ticket to code without a spec phase, the acceptance criteria embedded in the ticket body drift away from the project-level EARS spec. This gate provides a checkpoint that structurally guarantees "EARS-shaped requirements exist before implementation". The validation never uses an LLM; it is purely deterministic regex + file inspection.

## User-visible Behavior

### Gating flow

1. **Subscription**: at daemon startup, a subscriber is registered for `Queued -> Active` (vetoable) (the hook from [04-state-machine-and-recovery](04-state-machine-and-recovery.md)).
2. **Trigger**: the orchestrator publishes a `Queued -> Active` event.
3. **Materialization turn**: the gate invokes one constrained turn against the agent session for `(repo, issue)`.
   - The sole purpose of the turn: produce `.kiro/specs/<issue>/requirements.md` by merging the Linear ticket and the project's existing EARS docs under `.kiro/specs/`.
   - On the agent side, the kiro-discovery skill auto-invokes (description match) to perform the merge.
   - The daemon does not embed any skill prompt.
4. **Validation**: after the turn completes, the daemon mechanically inspects the artifact (see below).
5. **Decision**:
   - **Pass** → `Allow` to the orchestrator, transition to `Active`.
   - **Fail (retry remaining)** → `Deny`, retry.
   - **Fail (cap exhausted)** → `Deny` + escalation event.

### Validation rules (no LLM)

The exact path and required elements of `requirements.md` live in [`docs/reference/artifacts.md`](../reference/artifacts.md).
Pass conditions (all must hold):

1. `.kiro/specs/<issue>/requirements.md` exists
2. The file is non-empty
3. The encoding is sane
4. At least one EARS trigger keyword (`WHEN` / `IF` / `WHILE` / `WHERE` / `SHALL`) appears at an acceptance-criteria position

### Time bounds and retry

- **Per-attempt timeout**: a turn that exceeds `extension.gates.spec.timeout_ms` is terminated and the attempt fails.
- **Attempt cap**: `extension.gates.spec.max_attempts`. After the cap is reached, no further attempt is launched for the same `(repo, issue)`.
- **Total time bound**: `timeout_ms × max_attempts + a small documented overhead`.
- **Cap exhaustion**: deny + escalation event (issue / final attempt index / final reason / applied `required_status`).

### Concurrency and idempotency

- Concurrent evaluations of the same `(repo, issue)` are serialized.
- If a `Queued -> Active` event is duplicated by webhook redelivery, etc., it is treated as the same logical attempt and does not double-consume `max_attempts`.
- Per-`(repo, issue)` independence (one slow / failing evaluation does not affect others).
- If the daemon crashes during evaluation, in-flight attempts are treated as failed at the next start, and a fresh attempt is restarted with the remaining budget restored by reconciliation.

### Self-diagnosis from the agent

The `kiro_spec_status` agent tool (described in detail in [11-agent-tool-boundary](11-agent-tool-boundary.md)) returns gate state read-only:

- artifact path / present flag
- latest validation outcome
- attempt count, remaining attempts

## Capabilities

- **Mechanical validation only**: no LLM API, no semantic analyzer, no external service.
- **Fail-closed**: if the gate itself raises an unhandled error, deny.
- **Artifact constraint**: artifacts produced anywhere other than the expected path do not satisfy the gate.
- **Defaulted-key logging**: when keys under `extension.gates.spec.*` fall back to defaults, log them.
- **Hot reload**: changing `extension.gates.spec.*` in `WORKFLOW.md` applies the new values from the next attempt.
- **Reuse of the roki-mvp pipeline**: every event flows through roki-mvp's tracing pipeline + redaction layer ([13-observability-logs](13-observability-logs.md)). The spec gate has no dedicated destination.

## Boundaries

- **Deep semantic EARS validation** is out of scope (only regex shape).
- **Quality judgment of agent output** is the responsibility of the agent / kiro-discovery skill.
- **Cross-spec consistency checks** (e.g. whether this ticket's requirements contradict project-level EARS) are out of scope (a future `roki-spec-sync` will own this).
- **Transitions other than `Queued -> Active`** are not touched.
- **Persistent attempt history** is not maintained (in-memory + log only).
- **No mutating action is exposed to the agent** (`kiro_spec_status` is read-only).

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-spec-gate`; Boundary Strategy > "kiro gates"
- **Requirements**:
  - `roki-spec-gate Req 1` - `Req 9`: subscription, materialization, validation, retry, status tool, configuration, concurrency, observability
  - `roki-mvp Req 8.3`: existence of vetoable hooks
  - `roki-mvp Req 6.5`: WORKFLOW.md extension namespaces
- **Design**:
  - `.kiro/specs/roki-spec-gate/design.md`
- **Related reference**: [artifacts.md](../reference/artifacts.md) (`requirements.md`), [config.md](../reference/config.md) (`extension.gates.spec.*`), [log-events.md](../reference/log-events.md) (gate events), [extension-surface.md](../reference/extension-surface.md) (vetoable hook)
- **Related FR**: 04-state-machine-and-recovery, 02-configuration, 11-agent-tool-boundary, 13-observability-logs
