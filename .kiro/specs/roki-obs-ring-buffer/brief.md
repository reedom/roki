---
refs:
  id: brief:roki-obs-ring-buffer
  kind: brief
  title: "roki-obs-ring-buffer Brief"
  spec: roki-obs-ring-buffer
---

# Brief: roki-obs-ring-buffer

## Problem

Live HTTP / TUI consumers need a low-overhead tail of recent events without rereading the file.

## Desired Outcome

Bounded in-memory ring buffer (`[log].ring_size` entries) of recent events; read-only feed for `/api/v1/events` and TUI events view.

## Scope

- **In**: ring data structure (lock-free or mutex-bounded); subscriber API; cursor / since-id semantics.
- **Out**: persistence; cross-process sharing.

## Dependencies

- roki-obs-tracing-pipeline

## Critical FR references

- fr:08-observability-logs
- fr:10-http-api
- fr:11-roki-tui
