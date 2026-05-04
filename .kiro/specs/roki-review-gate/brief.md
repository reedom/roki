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
Mirror roki-spec-gate's pattern: a daemon-side gate hooked into the worker's clean-exit `Active -> Inactive` seam. The worker session produces `review.md` inside the bounded `claude` invocation via the kiro skill set (per-task `kiro-review`, feature-level `kiro-validate-impl`, fresh-evidence `kiro-verify-completion`) before clean exit. Daemon then verifies the review artifact's structure + pass status. Pass = transition allowed; fail = `Deny+RetryWithContext(payload)` re-launches the worker with findings injected via `additional_context`, up to `max_attempts`.

## Scope
- **In**:
  - State-machine hook into roki-mvp at the worker's clean-exit `Active -> Inactive` seam
  - Structured artifact validation of `review.md` produced by the worker session (per-criterion pass / fail + code references)
  - `kiro_review_status` agent-side read-only tool
  - `WORKFLOW.md` keys: `gates.review.required_status`, `gates.review.max_attempts`
  - "Fix-finding" feedback loop: failed review re-launches the worker with findings injected via `additional_context`, up to `max_attempts` (default 3)
  - Escalation through linear-updater + the TUI escalation queue when gate fails after `max_attempts`

- **Out**:
  - Deciding what counts as "code evidence" beyond presence of file references in the artifact (substantive judgment lives in the kiro skill set running inside the worker session)
  - Auto-merge or auto-PR-open (the agent still drives Linear / `gh`; the daemon only gates the transition)
  - Any daemon-launched `claude` subprocess for review (the worker session produces `review.md` before clean exit)

## Boundary Candidates
- **Gate validation vs substantive judgment**: presence / structure check daemon-side; substantive judgment kiro skill set inside the worker.
- **Fix-finding loop vs gate**: the loop is part of the gate's lifecycle, but the re-launch + `additional_context` forwarding belong to the engine adapter (roki-mvp).

## Out of Boundary
- Implementation phase (roki-mvp).
- Spec materialization (roki-spec-gate).
- Auto-merge logic.

## Upstream / Downstream
- **Upstream**: roki-mvp (state machine, claude session, tool registry, engine adapter `additional_context` channel, linear-updater dispatch); roki-spec-gate (review reads spec criteria from `.kiro/specs/<issue>/requirements.md`).
- **Downstream**: none in MVP (post-merge distill is handled in CI, not by the daemon).

## Existing Spec Touchpoints
- **Extends**: roki-mvp.
- **Adjacent**: roki-spec-gate.

## Constraints
- Review artifact path / schema must be stable so consumers can rely on it across spec versions.
- Re-launch loop must respect roki-mvp's overall worker `max_turns` budget; gate cannot exceed that.
- No per-attempt `timeout_ms`: time-boundedness is roki-mvp's `max_turns` and stall detection on the same worker subprocess that produces `review.md`.
