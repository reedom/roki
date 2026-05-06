---
refs:
  id: brief:roki-obs-event-catalog
  kind: brief
  title: "roki-obs-event-catalog Brief"
  spec: roki-obs-event-catalog
---

# Brief: roki-obs-event-catalog

## Problem

Event names and fields are ad hoc; consumers need a stable contract.

## Desired Outcome

The structured event catalog of design §8.5: enumerated event names with required fields, emitted by the daemon at fixed sites (admission, cycle start / end, phase start / end, directive parse, failure detected, escalation queued, etc.). Phases never emit events into this catalog directly.

## Scope

- **In**: enumerated catalog; emit-site discipline; field-shape stability guarantees.
- **Out**: phase-side events (operator concern); reference doc generation.

## Dependencies

- roki-obs-tracing-pipeline

## Critical FR references

- fr:08-observability-logs
