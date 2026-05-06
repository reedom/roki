---
refs:
  id: brief:roki-config-hot-reload
  kind: brief
  title: "roki-config-hot-reload Brief"
  spec: roki-config-hot-reload
---

# Brief: roki-config-hot-reload

## Problem

Operator policy edits (`WORKFLOW.toml`, `workflow/*.md`) need a daemon restart.

## Desired Outcome

Per design §3.7: file-watch triggers reload + schema validate. On success, the next webhook uses the new policy; in-flight cycles continue with their pre-reload policy. On failure, keep the previous policy and log the offending field. Per-key invalidity inside `when.*` rejects the whole entry; the entry is treated as if it did not match.

## Scope

- **In**: notify-rs watcher; reload guard; per-key invalidity handling; structured failure log.
- **Out**: `roki.toml` hot reload (restart-only by design).

## Dependencies

- roki-config-workflow-toml-full

## Critical FR references

- fr:02-configuration
