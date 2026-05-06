---
refs:
  id: brief:roki-cli-daemon
  kind: brief
  title: "roki-cli-daemon Brief"
  spec: roki-cli-daemon
---

# Brief: roki-cli-daemon

## Problem

Skeleton has only the `roki` start path; production needs ergonomic flags, graceful shutdown, and config-path resolution.

## Desired Outcome

`roki` (start daemon, foreground) with documented `--config <path>` flag and env override; SIGTERM / SIGINT trigger graceful shutdown (drain in-flight cycles up to a configured deadline, then SIGTERM phase subprocesses).

## Scope

- **In**: argv parser; lifecycle hooks (start, drain, terminate); config-path resolution order (flag > env > default).
- **Out**: subcommands (separate Wave 6 specs); daemonize / detach.

## Dependencies

- roki-skeleton

## Critical FR references

- fr:12-daemon-lifecycle
