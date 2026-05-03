---
refs:
  id: brief:roki-review-gate
  kind: brief
  title: "roki-review-gate Brief"
  spec: roki-review-gate
---

# Brief: roki-review-gate

## Problem
A finished implementation may still violate acceptance criteria. Without a deterministic review checkpoint, a "pass" depends on the agent's self-assessment, which can drift from EARS criteria. Symphony has no equivalent gate -- they rely on prompt convention. roki should turn this into a daemon-enforced contract.

## Current State
- roki-mvp ends when the agent moves the ticket to a handoff state (e.g., `In Review`).
- Without a review gate, that transition reflects only the agent's belief that work is done.
- monorail had a planned `/monorail-verify-acceptance` step agent for this; it was never built.

## Desired Outcome
- Before the daemon allows the ticket to transition to `In Review` (or whatever state opens a PR for human review), it requires a kiro-review artifact attesting that EARS criteria are satisfied with code-level evidence.
- The artifact lives at a known path inside the workspace (e.g., `.kiro/specs/<issue>/review.md`) and contains a structured pass / fail per EARS bullet plus code references.
- Failure routes the worker back to a "fix-finding" turn rather than letting the transition through.

## Approach
Mirror roki-spec-gate's pattern: a daemon-side gate hooked into the pre-`In Review` state-machine seam. The gate triggers a constrained "review turn" that invokes the kiro-review skill with the spec criteria and the implementation diff as inputs. Daemon then verifies the review artifact's structure + pass status. Pass = transition allowed; fail = re-run implementation phase with findings as input, up to `max_attempts`.

## Scope
- **In**:
  - State-machine hook into roki-mvp at the pre-`In Review` seam
  - Review-turn invocation (constrained turn purpose: produce `.kiro/specs/<issue>/review.md`)
  - Structured artifact validation (per-criterion pass / fail + code references)
  - `kiro_review_status` agent-side read-only tool
  - `WORKFLOW.md` keys: `gates.review.required_status`, `gates.review.timeout_ms`, `gates.review.max_attempts`
  - "Fix-finding" feedback loop: failed review re-enters implementation phase with findings as additional context, up to `max_attempts` (default 3)
  - Escalation event when gate fails after `max_attempts`

- **Out**:
  - Deciding what counts as "code evidence" beyond presence of file references in the artifact (LLM judgment lives in the review turn)
  - Auto-merge or auto-PR-open (the agent still drives Linear / `gh`; the daemon only gates the transition)

## Boundary Candidates
- **Gate orchestration vs review turn**: gate is supervisor; turn is the work.
- **Validation vs scoring**: presence / structure check daemon-side; substantive judgment agent-side.
- **Re-implementation feedback loop vs gate**: the loop is part of the gate's lifecycle, but its prompt-injection mechanics belong to the engine adapter (roki-mvp).

## Out of Boundary
- Implementation phase (roki-mvp).
- Spec materialization (roki-spec-gate).
- Auto-merge logic.

## Upstream / Downstream
- **Upstream**: roki-mvp (state machine, claude session, tool registry); roki-spec-gate (review reads spec criteria from `.kiro/specs/<issue>/requirements.md`).
- **Downstream**: roki-distill-postmerge (uses `review.md` as one input for archive / distill decisions).

## Existing Spec Touchpoints
- **Extends**: roki-mvp.
- **Adjacent**: roki-spec-gate.

## Constraints
- Review artifact path / schema must be stable so distill-postmerge can rely on it.
- Re-implementation loop must respect roki-mvp's overall worker `max_turns` budget; gate cannot exceed that.
- Gate must be time-bounded: never block indefinitely; escalate on `timeout_ms`.
