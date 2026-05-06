---
refs:
  id: brief:roki-linear-cold-start
  kind: brief
  title: "roki-linear-cold-start Brief"
  spec: roki-linear-cold-start
---

# Brief: roki-linear-cold-start

## Problem

Daemon restart leaves orphan worktrees and missed assignments; restart-recovery via Linear + filesystem (no DB) needs an explicit reconcile step.

## Desired Outcome

At daemon start: fetch all assigned admitted tickets via Linear API; reconcile against on-disk worktrees and session_tempdirs under `[paths]`. Tickets present on disk but no longer admitted → orphan delete. Tickets admitted but no worktree → wait for the next webhook / first run-phase.

## Scope

- **In**: Linear list call; on-disk scan; orphan classify + delete; idempotent re-run.
- **Out**: persistent state file (none); polling cadence (lives with diff cache / poll fallback).

## Dependencies

- roki-linear-admission-repos
- roki-runtime-worktree-lazy

## Critical FR references

- fr:07-recovery
