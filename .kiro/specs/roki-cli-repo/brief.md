---
refs:
  id: brief:roki-cli-repo
  kind: brief
  title: "roki-cli-repo Brief"
  spec: roki-cli-repo
---

# Brief: roki-cli-repo

## Problem

Phases need an unambiguous path to the worktree (or ghq base when not yet materialized).

## Desired Outcome

Per design §5.3: `roki repo [<ghq>] [--auto-clone] [--worktree]` outputs the resolved path on stdout. Defaults read `ROKI_TICKET_ID` / `ROKI_REPO` from env. `--worktree` requires the worktree to exist (exit 1 if not). `--auto-clone` runs `ghq get` when the ghq base is missing.

## Scope

- **In**: argv parser; resolution (worktree if present else ghq base); env-var defaults; `--auto-clone` shell-out.
- **Out**: worktree creation (`roki-runtime-worktree-lazy` owns it).

## Dependencies

- roki-runtime-worktree-lazy

## Critical FR references

- fr:05-worktree-and-session
