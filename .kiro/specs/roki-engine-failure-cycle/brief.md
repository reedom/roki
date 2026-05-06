---
refs:
  id: brief:roki-engine-failure-cycle
  kind: brief
  title: "roki-engine-failure-cycle Brief"
  spec: roki-engine-failure-cycle
---

# Brief: roki-engine-failure-cycle

## Problem

Daemon-detected failures have no operator hook.

## Desired Outcome

`[[on_failure]]` first-match against `failure.kind` (and optional phase scope) spawns a failure-handler cycle with `{{ failure.* }}` Liquid vars populated. Failures inside failure cycles do not chain; the default behavior (escalation entry only) bounds recovery depth.

## Scope

- **In**: six failure kinds (`process_crash | unparseable | schema_drift | stall | iter_exhausted | template_error`); failure-cycle dispatch; `{{ failure.* }}` template variable surface; on-no-match escalation queue entry.
- **Out**: kind detection (lives in the originating subsystems); persistent escalation queue (in-memory only by design).

## Dependencies

- roki-engine-iteration-loop

## Critical FR references

- fr:06-failure-handling
