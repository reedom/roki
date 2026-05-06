---
refs:
  id: brief:roki-engine-iter-cap
  kind: brief
  title: "roki-engine-iter-cap Brief"
  spec: roki-engine-iter-cap
---

# Brief: roki-engine-iter-cap

## Problem

The iteration loop can run forever without a cap.

## Desired Outcome

`[engine].max_iterations` (default 10) caps a cycle's iteration count. On hit: cooperative `iteration_exhausted` directive into the active session's stdin; SIGTERM after stall window if the AI does not emit `directive: end`; command-form phases route through `[[on_failure]] when.kind = "iter_exhausted"`.

## Scope

- **In**: counter; stdin write of `iteration_exhausted` to active sessions; SIGTERM fallback; `iter_exhausted` failure routing for command form.
- **Out**: stall detection itself (`roki-engine-stall`); `[[on_failure]]` machinery (`roki-engine-failure-cycle`).

## Dependencies

- roki-engine-iteration-loop

## Critical FR references

- fr:01-engine-model
- fr:06-failure-handling
