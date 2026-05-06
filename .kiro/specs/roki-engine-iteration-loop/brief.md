---
refs:
  id: brief:roki-engine-iteration-loop
  kind: brief
  title: "roki-engine-iteration-loop Brief"
  spec: roki-engine-iteration-loop
---

# Brief: roki-engine-iteration-loop

## Problem

Skeleton runs one phase and exits; there is no per-cycle loop.

## Desired Outcome

Cycle = pre → run → post; daemon parses the last JSON object on each phase's stdout as a `directive` and loops or terminates per design §4.2.

## Scope

- **In**: pre / run / post phase sequencing; legal directive sets (pre: `run|end`; post: `pre|run|end`); omitted-pre defaults to `run`, omitted-post defaults to `end`; last-JSON-object parser.
- **Out**: iteration cap, cleanup / failure cycles, stall, queue preemption (separate Wave 1 specs).

## Dependencies

- roki-skeleton

## Critical FR references

- fr:01-engine-model
- fr:04-phase-execution
