---
refs:
  id: brief:roki-skeleton
  kind: brief
  title: "roki-skeleton Brief"
  spec: roki-skeleton
---

# Brief: roki-skeleton

## Problem

The post-pivot daemon source tree is empty. Without a backbone, every later wave has nowhere to integrate.

## Desired Outcome

A walking-skeleton daemon boots and processes one Linear webhook through one rule cycle to a clean exit, pinned by `tests/e2e/skeleton_smoke.rs`. Every later spec must keep that smoke green.

## Scope

- **In**:
  - `roki` CLI start with `--config <path>`.
  - `roki.toml` read for `[linear]`, `[network]`, `[default.ai.command]`, `[paths]`, `[engine]`, `[log]`.
  - HTTP webhook receive on `[network].bind` / `port`. No HMAC verify.
  - Single-shot ticket cache (full diff cache lives in `roki-linear-diff-cache`).
  - Assignee filter (`[admission].assignee`; `me` resolves via Linear API token).
  - Hardcoded single-repo resolve: first `[[admission.repos]]` entry only; no `when.*` matchers.
  - `[[rule]]` first-match (status / labels equality only).
  - Cmd-form phase only (`run.cmd = "..."`); no `path`, no `prompt`.
  - stdout/stderr captured to per-cycle files under `[paths].session_root`.
  - Process exit on cycle end (single-cycle smoke).
- **Out**:
  - HMAC signature verify (`roki-linear-signature-verify`).
  - Full matcher vocabulary (`roki-linear-admission-repos`).
  - `[[cleanup]]` / `[[on_failure]]` (Wave 1).
  - Iteration loop directives (`roki-engine-iteration-loop`).
  - Session-mode phases (`roki-runtime-session-mode`).
  - Hot reload (`roki-config-hot-reload`).
  - HTTP API beyond webhook intake (Wave 7).
  - TUI (Wave 8).
  - CLI subcommands beyond `roki` (Wave 6).

## Dependencies

None.

## Critical FR references

- fr:01-engine-model
- fr:02-configuration
- fr:03-linear-admission
- fr:04-phase-execution
- fr:05-worktree-and-session
- fr:12-daemon-lifecycle
