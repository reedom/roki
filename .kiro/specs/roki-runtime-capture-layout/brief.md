---
refs:
  id: brief:roki-runtime-capture-layout
  kind: brief
  title: "roki-runtime-capture-layout Brief"
  spec: roki-runtime-capture-layout
---

# Brief: roki-runtime-capture-layout

## Problem

Per-cycle / per-phase stdout / stderr capture format is undefined. `roki log` has nothing stable to read.

## Desired Outcome

Documented file layout under `[paths].session_root` keyed by cycle id and phase: stdout, stderr, terminal-event payloads. Stable enough for `roki log` (Wave 6) to read by `--cycle <id>` / `--phase <name>`.

## Scope

- **In**: directory layout (e.g. `<ticket>/<cycle_id>/<phase>/{stdout,stderr,terminal}.log`); rotation / size cap policy.
- **Out**: `roki log` CLI itself (`roki-cli-log`); ring-buffer surface (Wave 5).

## Dependencies

- roki-skeleton

## Critical FR references

- fr:08-observability-logs
- fr:09-log-access-cli
