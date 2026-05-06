---
refs:
  id: brief:roki-runtime-worktree-lazy
  kind: brief
  title: "roki-runtime-worktree-lazy Brief"
  spec: roki-runtime-worktree-lazy
---

# Brief: roki-runtime-worktree-lazy

## Problem

Skeleton hardcodes worktree timing; need lazy materialization aligned with first run-phase.

## Desired Outcome

session_tempdir created at admission (logs need a place to land before any phase runs). Worktree created lazily on the first `pre.directive = run` of the ticket's first cycle. Both deleted on `[[cleanup]]` cycle completion, admission-filter eviction, or cold-start orphan reconciliation.

## Scope

- **In**: lazy worktree creation; `wt` / `ghq` shell-out; cleanup hook for both directories.
- **Out**: orphan reconciliation at daemon start (`roki-linear-cold-start`).

## Dependencies

- roki-skeleton

## Critical FR references

- fr:05-worktree-and-session
