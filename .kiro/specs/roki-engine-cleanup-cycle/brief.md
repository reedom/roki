---
refs:
  id: brief:roki-engine-cleanup-cycle
  kind: brief
  title: "roki-engine-cleanup-cycle Brief"
  spec: roki-engine-cleanup-cycle
---

# Brief: roki-engine-cleanup-cycle

## Problem

Skeleton has no cleanup path on terminal status (`Done`, `Cancelled`, label gone).

## Desired Outcome

`[[cleanup]]` first-match runs before `[[rule]]` on every diff. A cleanup entry with all phases omitted is shorthand for "delete worktree + session_tempdir immediately, no cycle".

## Scope

- **In**: cleanup-before-rule eval order; immediate-delete shorthand; full pre/run/post cycle when phases are present; daemon-driven worktree + session_tempdir delete on cycle end.
- **Out**: failure cycles (`roki-engine-failure-cycle`); orphan reconciliation at start (`roki-linear-cold-start`).

## Dependencies

- roki-engine-iteration-loop

## Critical FR references

- fr:01-engine-model
- fr:05-worktree-and-session
