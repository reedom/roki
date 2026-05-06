---
refs:
  id: brief:roki-config-workflow-toml-full
  kind: brief
  title: "roki-config-workflow-toml-full Brief"
  spec: roki-config-workflow-toml-full
---

# Brief: roki-config-workflow-toml-full

## Problem

Skeleton parses only minimal `[[rule]]` and `[[admission.repos]]` entries.

## Desired Outcome

Full `WORKFLOW.toml` schema (admission + rule + cleanup + on_failure) with first-match semantics; AND-within-entry, OR-via-additional-entries; full condition vocabulary per design §3.5; phase specification (`path` / `prompt` / `cmd`) per §3.6; structured validation errors with field locations.

## Scope

- **In**: schema definition; toml→AST; per-section parser; condition vocabulary; phase-spec validation.
- **Out**: hot reload (`roki-config-hot-reload`); per-repo TOML body (`roki-config-per-repo-toml`).

## Dependencies

- roki-skeleton

## Critical FR references

- fr:02-configuration
