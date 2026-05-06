---
refs:
  id: brief:roki-cli-log
  kind: brief
  title: "roki-cli-log Brief"
  spec: roki-cli-log
---

# Brief: roki-cli-log

## Problem

Operators have no first-class way to read captured phase output.

## Desired Outcome

`roki log --cycle <id> [--phase pre|run|post] [--stream stdout|stderr]` reads files under the capture layout and writes to the operator's stdout. Read-only operation; no daemon connection required.

## Scope

- **In**: argv parser; capture-layout path resolution from `[paths].session_root`; file read; stream selection.
- **Out**: ring-buffer access (`roki-cli-events`); live tailing across daemon restart.

## Dependencies

- roki-runtime-capture-layout

## Critical FR references

- fr:09-log-access-cli
