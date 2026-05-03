---
refs:
  id: fr:09-pre-pr-gate
  kind: fr
  title: "Pre-PR Gate"
  spec: roki-review-gate
  implements:
    - requirements:roki-review-gate
---

# FR 09: Pre-PR Gate

> Gate `AwaitingReview -> TerminalSuccess` with a vetoable hook. Do not let it reach `TerminalSuccess` without a structured `review.md` (per-criterion pass + code references). On failure, run a fix loop that returns to `Active` with the findings as context.

## Purpose

Trusting only the agent's self-assessment of "done" lets a ticket reach a PR-ready state without satisfying the EARS acceptance criteria. This gate provides a checkpoint that structurally guarantees "a structurally-validated review exists after implementation". Validation is structural only (file presence / schema / per-criterion status / code-reference reachability) — no LLM is used. On failure, the failed findings are passed back to the agent so it can fix them.

## User-visible Behavior

### Gating flow

1. **Subscription**: at daemon startup, register a subscriber for `AwaitingReview -> TerminalSuccess` (vetoable) ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)).
2. **Trigger**: the orchestrator publishes an `AwaitingReview -> TerminalSuccess` event.
3. **Review turn**: if no usable review artifact exists, request that the engine adapter launch a constrained review turn.
   - Declared purpose of the turn: produce `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md`.
   - Inputs: `requirements.md` (the spec criteria written by [08-pre-implementation-gate](08-pre-implementation-gate.md)) and the implementation diff produced by the active phase.
   - Skill: kiro-review skill auto-invokes (description match). The daemon does not embed any skill prompt.
   - Serialization with the implementation phase: while the review turn is running, the implementation phase for the same `(repo, issue)` does not run.
4. **Validation**: after the turn completes, the daemon inspects the artifact structurally (see below).
5. **Decision**:
   - **Pass** → `Allow` to the orchestrator, commit to `TerminalSuccess`.
   - **Fail (retry remaining)** → enter the fix-finding loop.
   - **Fail (cap exhausted)** → `Deny` + request transition to `TerminalFailure` + escalation event.

### Artifact path and schema

The exact path and required fields of `review.md` are in [`docs/reference/artifacts.md`](../reference/artifacts.md).
Highlights:

- **Path**: `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` (stable; never changed across versions).
- **Schema (required)**: an overall status (`pass`/`fail`) + per-criterion entries (each numeric requirement ID gets a status) + at least one on-disk reachable code reference for each `pass` entry.
- **Publication site**: the schema is documented in [`docs/reference/artifacts.md`](../reference/artifacts.md) and `SPEC.md` (downstream consumers such as [10-post-merge-distill](10-post-merge-distill.md) depend on this).

### Validation rules (no LLM)

The full list of failure codes lives in the "Required elements of review.md" section of [`docs/reference/artifacts.md`](../reference/artifacts.md) (`fail-missing` / `fail-schema` / `fail-evidence` / `fail-timeout` / `fail-missing-spec`).
Substantive judgment of "does the code actually satisfy the criterion" is the responsibility of the review turn (kiro-review skill). The daemon performs only structural checks.

### Fix-finding loop

- **Trigger**: failing gate result + attempt counter < `extension.gates.review.max_attempts` (default 3).
- **Behavior**:
  - Move the issue from `AwaitingReview` back to `Active`.
  - Increment the attempt counter for `(repo, issue)`.
  - Inject the **failing per-criterion entries** (criterion id, fail reason, the diagnostic text from the review turn) into the next worker invocation through the engine adapter's `additional_context` channel ([12-extension-surface](12-extension-surface.md)). It lives in a region separate from the worker template body, and the daemon does not interpret its contents.
- **Worker turn budget**: the fix loop honors the per-worker `max_turns` budget owned by roki-mvp (the gate does not consume a separate budget).
- **Counter reset**: when an entry to `AwaitingReview` happens via a non-veto path (e.g. operator-driven retry), the attempt counter is reset.

### Time bounds and escalation

- **Per-attempt timeout**: `extension.gates.review.timeout_ms`.
- **Escalation event** (on cap exhaustion / on `fail-missing-spec`): `(repo, issue)` / attempt count / final gate result code / final failure reason.

### Self-diagnosis from the agent

The `kiro_review_status` agent tool (described in detail in [11-agent-tool-boundary](11-agent-tool-boundary.md)) returns gate state read-only:

- artifact presence flag, latest gate result, current attempt counter, configured `max_attempts`, latest failure reason

## Capabilities

- **Mechanical validation only**: no LLM, no heuristic substring search.
- **Fail-closed**: an internal error in the gate is `Deny`.
- **Stable artifact path**: never changed across spec versions (so downstream consumers can depend on it).
- **Spec-missing fast-fail**: a missing `requirements.md` goes straight to escalation.
- **Hot reload**: changes to `extension.gates.review.*` apply from the next attempt; in-flight attempt counters are not retroactively reset.
- **Reuse of the roki-mvp pipeline**: structured logs flow through the roki-mvp tracing + redaction layer ([13-observability-logs](13-observability-logs.md)).

## Boundaries

- **Substantive judgment** (does the code satisfy the criterion) is the agent's responsibility (kiro-review skill).
- **PR operations / merge orchestration / Linear writes** are out of scope (the gate only vetoes; the agent handles PRs / Linear).
- **Multi-turn review sessions** are out of scope (1 attempt = 1 turn).
- **Persistent review history** is not maintained (logs only; the artifact is the latest one).
- **Cross-issue review correlation** is out of scope.

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-review-gate`; Boundary Strategy > "kiro gates"
- **Requirements**:
  - `roki-review-gate Req 1` - `Req 8`: subscription / artifact / validation / review turn / fix loop / config / status tool / escalation
  - `roki-mvp Req 8.3`, `Req 13.4`: vetoable hook, `additional_context` channel
- **Design**:
  - `.kiro/specs/roki-review-gate/design.md`
- **Related reference**: [artifacts.md](../reference/artifacts.md) (`review.md` schema), [config.md](../reference/config.md) (`extension.gates.review.*`), [extension-surface.md](../reference/extension-surface.md) (vetoable hook, `additional_context`), [log-events.md](../reference/log-events.md) (gate events)
- **Related FR**: 04-state-machine-and-recovery, 02-configuration, 08-pre-implementation-gate, 11-agent-tool-boundary, 12-extension-surface, 13-observability-logs
