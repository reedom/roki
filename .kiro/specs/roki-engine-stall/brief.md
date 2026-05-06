---
refs:
  id: brief:roki-engine-stall
  kind: brief
  title: "roki-engine-stall Brief"
  spec: roki-engine-stall
---

# Brief: roki-engine-stall

## Problem

A subprocess that stops emitting on stdout deadlocks the daemon.

## Desired Outcome

Each subprocess has a stall window: `[default.ai.session].stall_seconds` for sessions, `[default.ai.command].stall_seconds` for commands; frontmatter on `workflow/*.md` overrides per file. On stdout silence beyond the window the daemon SIGTERMs and routes through `[[on_failure]] when.kind = "stall"`.

## Scope

- **In**: timer reset on stdout activity; SIGTERM on expiry; failure-kind emission as `stall`.
- **Out**: failure-cycle dispatch (`roki-engine-failure-cycle`); per-cycle / per-iter custom windows.

## Dependencies

- roki-engine-iteration-loop

## Critical FR references

- fr:06-failure-handling
- fr:04-phase-execution
