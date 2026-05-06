---
refs:
  id: brief:roki-runtime-template-vars
  kind: brief
  title: "roki-runtime-template-vars Brief"
  spec: roki-runtime-template-vars
---

# Brief: roki-runtime-template-vars

## Problem

Phase bodies need typed access to ticket / cycle / iter context.

## Desired Outcome

Liquid render of phase bodies and inline `prompt` / `cmd` strings with the variable surface of design §4.4: `ticket.* / repo.* / cycle.* / pre.* / post.* / run.* / failure.*`. Matching `ROKI_*` env vars are injected for shell-form phases. Render failure raises `template_error`.

## Scope

- **In**: variable population from in-memory ticket / cycle / iter state; env var injection; `template_error` failure-kind emission.
- **Out**: variable schema evolution; user-defined helpers.

## Dependencies

- roki-skeleton

## Critical FR references

- fr:04-phase-execution
