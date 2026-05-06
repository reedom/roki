---
refs:
  id: brief:roki-cli-events
  kind: brief
  title: "roki-cli-events Brief"
  spec: roki-cli-events
---

# Brief: roki-cli-events

## Problem

Operators have no CLI to tail the structured event log.

## Desired Outcome

`roki events --since <ts> [--follow]` reads the ring buffer (when daemon up) or the file destination configured by `[log]`, emitting JSONL on stdout. Follow mode subscribes to new entries.

## Scope

- **In**: argv parser; ring-buffer client / file reader; cursor / since-ts semantics; follow mode.
- **Out**: structured filter DSL (post-MVP).

## Dependencies

- roki-obs-ring-buffer

## Critical FR references

- fr:09-log-access-cli
