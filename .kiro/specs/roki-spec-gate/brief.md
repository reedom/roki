---
refs:
  id: brief:roki-spec-gate
  kind: brief
  title: "roki-spec-gate Brief"
  spec: roki-spec-gate
---

# Brief: roki-spec-gate

## Problem
Without an explicit spec phase, the agent jumps straight from a Linear ticket to code. Acceptance criteria written in the ticket body drift from project-level EARS specs (managed under `.kiro/specs/`), and the agent loses the chance to align them before implementation. Result: PRs that satisfy the ticket but violate adjacent project requirements, or PRs that re-litigate decisions already made in older specs.

## Current State
- roki-mvp gives us the symphony-parity pipeline, but no kiro-aligned phase.
- Discovery confirmed pre-implementation distill should merge ticket EARS with existing project EARS docs, driven by the kiro-discovery skill.
- monorail had a planned but unbuilt `/monorail-ears` distill command; that maps directly into this gate.

## Desired Outcome
- Before any implementation work begins, the daemon refuses to transition the ticket to `In Progress` until `.kiro/specs/<issue>/requirements.md` exists with EARS-shaped acceptance criteria, materialized by a kiro-discovery flow inside the agent session.
- The agent invokes kiro-discovery (via skill auto-invocation or explicit `--agents` step agent) to: read the Linear ticket body, scan project-level EARS docs, produce a merged spec, write it under `.kiro/specs/<issue>/`.
- The daemon's gate is binary: pass = transition allowed; fail = stay in current state, surface escalation.

## Approach
A daemon-side gate hooked into roki-mvp's state-machine extension points. The gate runs after the workspace is provisioned and before the implementation phase prompt fires. It triggers a roki-internal "spec materialization turn" -- a constrained agent invocation whose only job is to produce the spec artifacts. Daemon then verifies file presence + EARS shape; pass advances state, fail records escalation. The gate exposes a read-only `kiro_spec_status` tool to the agent so subsequent turns can self-check.

## Scope
- **In**:
  - State-machine hook into roki-mvp at the pre-implementation seam
  - Spec-materialization phase invocation (constrained turn purpose: produce `.kiro/specs/<issue>/requirements.md`)
  - File-existence + minimal EARS-shape validation (presence of `WHEN` / `IF` / `WHILE` constructs in `requirements.md`)
  - `kiro_spec_status` agent-side read-only tool
  - `WORKFLOW.md` keys: `gates.spec.required_status`, `gates.spec.timeout_ms`, `gates.spec.max_attempts`
  - Escalation event when gate fails after `max_attempts`

- **Out**:
  - Deep semantic validation of EARS bullets (presence + structure check is enough)
  - Auto-rewriting `requirements.md` from outside the agent session
  - Project-level EARS sync across multiple specs (deferred; could be a `roki-spec-sync` future spec)

## Boundary Candidates
- **Gate orchestration vs spec-materialization turn**: the gate is the supervisor; the turn is the work.
- **Validation vs the agent's authoring**: validation is daemon-side; authoring is always agent-side.

## Out of Boundary
- Implementation phase (lives in roki-mvp).
- Review gate (separate spec).
- Multi-spec project sync.

## Upstream / Downstream
- **Upstream**: roki-mvp (state machine, workspace, claude session, agent tool registry).
- **Downstream**: roki-review-gate (review may want to read spec status); roki-distill-postmerge (post-merge sweep operates on `.kiro/specs/<issue>/`).

## Existing Spec Touchpoints
- **Extends**: roki-mvp (adds gate hook, adds agent tool, adds `WORKFLOW.md` keys).
- **Adjacent**: roki-review-gate (parallel sibling).

## Constraints
- Gate must be time-bounded: failure to produce a spec within `timeout_ms` advances to escalation, never blocks indefinitely.
- Validation must not require LLM judgment (file presence + regex is enough); LLM-side judgment lives in the materialization turn itself.
- Pre-impl distill (ticket EARS + project EARS merge) is implemented inside the materialization turn, not as a separate phase.
