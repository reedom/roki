# Brief: roki-distill-postmerge

## Problem
Each ticket leaves behind a trail of "flow-type" documents: kiro `design.md` and `tasks.md`, superpowers specs, plan command outputs, scratch notes. These have served their purpose by the time the PR merges. Left alone they accumulate noise; deleted blindly they lose useful long-term context. We need a deliberate sweep that classifies each artifact and routes it.

## Current State
- monorail had `/monorail-ears-distill-command` planned (post-PR distillation of spec docs) but unbuilt.
- Discovery expanded scope: distill should handle not only kiro outputs but also superpowers specs, plan outputs, and "various" flow docs. Each artifact gets one of three dispositions: delete, archive, or distill into canonical EARS / product spec.
- roki-mvp + roki-review-gate make the merge event observable and the spec / review artifacts present.

## Desired Outcome
- After a PR merges (detected via `gh pr view --json mergedAt`), the daemon triggers a post-merge sweep turn.
- The sweep enumerates artifacts under the workspace's `.kiro/specs/<issue>/`, plus configured paths (e.g., `.superpowers/specs/`, `plans/`, `notes/`), classifies each per `WORKFLOW.md` rules + agent judgment, and routes:
  - **delete**: ephemeral artifacts that served the run (e.g., `tasks.md` once tasks are done)
  - **archive**: keep verbatim under a project archive path with a manifest
  - **distill**: extract canonical content into a stable home (e.g., requirements distilled into project-level EARS doc, design decisions distilled into `docs/decisions/`)
- The sweep is a separate phase the daemon orchestrates; it does not block PR merge or Linear `Done` transition.

## Approach
A post-terminal-state phase added to roki-mvp's state machine. Triggered when the worker observes a Linear `Done` (or detects merge via `gh`). A constrained sweep turn invokes a kiro / superpowers skill (TBD in design phase) that walks the configured paths, applies `WORKFLOW.md` `distill.routes` rules with agent judgment as the tie-breaker, and writes / deletes / archives accordingly. Daemon validates the audit manifest (list of moved / deleted / distilled files) before marking the ticket fully complete.

## Scope
- **In**:
  - Post-merge state-machine phase hook into roki-mvp
  - Configurable artifact discovery paths (`distill.paths` in `WORKFLOW.md`)
  - Classification rules (`distill.routes`) with agent-judgment fallback
  - Three dispositions: delete, archive, distill
  - Audit manifest written to `.kiro/specs/<issue>/distill-manifest.json`
  - Stable archive path scheme (e.g., `.kiro/archive/<issue>/`)
  - Daemon validation of manifest schema before terminal cleanup

- **Out**:
  - Cross-issue spec consolidation (distilling many issues' EARS into project-level EARS as a separate operation; deferred)
  - Auto-PR for distilled outputs (if distill produces commit-worthy changes, that's a separate PR opened by the agent in a follow-up turn -- outside this spec)
  - Real-time distill during the run (this spec is post-merge only)

## Boundary Candidates
- **Phase orchestration vs sweep turn**: phase is daemon-side; sweep is agent-side.
- **Routing rules vs judgment**: rules in `WORKFLOW.md` handle the obvious cases; agent decides ambiguous ones.
- **Manifest validation vs disposition execution**: validation is daemon-side (schema + path safety); execution is agent-side (file moves, archive writes).

## Out of Boundary
- Pre-implementation distill (roki-spec-gate handles the EARS-merge case for the pre-impl side).
- Cross-issue / project-level distillation passes.
- Auto-commit of distilled outputs.

## Upstream / Downstream
- **Upstream**: roki-mvp (state machine + workspace); roki-review-gate (`review.md` is one input artifact).
- **Downstream**: future cross-issue distill spec; future docs-site generator.

## Existing Spec Touchpoints
- **Extends**: roki-mvp (adds post-terminal phase).
- **Adjacent**: roki-spec-gate (pre-impl side of distill); roki-review-gate (consumes `review.md`).

## Constraints
- Sweep must be idempotent: re-running on an already-distilled issue is a no-op.
- Manifest schema must be stable + version-tagged for forward compat.
- Default behavior should be conservative: when in doubt, archive (not delete or distill).
- Path safety: no writes outside workspace + configured project archive root.
