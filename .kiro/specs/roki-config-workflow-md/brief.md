---
refs:
  id: brief:roki-config-workflow-md
  kind: brief
  title: "roki-config-workflow-md Brief"
  spec: roki-config-workflow-md
---

# Brief: roki-config-workflow-md

## Problem

Phase bodies referenced by `path = "workflow/*.md"` need a defined loader.

## Desired Outcome

`workflow/*.md` loader: YAML frontmatter (`session` / `command` (default `session`); optional `cli` for command form; optional `stall_seconds` override) + Liquid body. Frontmatter validation surfaces structured errors.

## Scope

- **In**: frontmatter parser; body extraction; Liquid render path; per-file `cli` / `stall_seconds` override.
- **Out**: hot reload (`roki-config-hot-reload`); cross-file include / partials.

## Dependencies

- roki-config-workflow-toml-full

## Critical FR references

- fr:02-configuration
- fr:04-phase-execution
