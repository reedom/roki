---
refs:
  id: brief:roki-obs-tracing-pipeline
  kind: brief
  title: "roki-obs-tracing-pipeline Brief"
  spec: roki-obs-tracing-pipeline
---

# Brief: roki-obs-tracing-pipeline

## Problem

Skeleton has minimal logging; production needs configurable structured tracing.

## Desired Outcome

`tracing` crate layers configured from `[log]`: level (`info` / `debug` / etc.), destination (`stdout` / `file` / `both`), `file_path`, JSONL line format on file destinations.

## Scope

- **In**: `tracing-subscriber` init; layer composition; rotation policy.
- **Out**: event catalog (`roki-obs-event-catalog`); ring buffer (`roki-obs-ring-buffer`); redaction (`roki-obs-redaction`).

## Dependencies

- roki-skeleton

## Critical FR references

- fr:08-observability-logs
