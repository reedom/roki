---
refs:
  id: brief:roki-runtime-session-mode
  kind: brief
  title: "roki-runtime-session-mode Brief"
  spec: roki-runtime-session-mode
---

# Brief: roki-runtime-session-mode

## Problem

Skeleton supports only one-shot command-form phases; no long-lived AI session reuse within a cycle.

## Desired Outcome

`session`-mode phases reuse one `--input-format stream-json --output-format stream-json` subprocess across pre / run / post within the same cycle. CLI from `[default.ai.session].cli` or per-file frontmatter `cli` override.

## Scope

- **In**: spawn from session CLI; stdin/stdout JSON multiplex; cycle-scoped lifetime; per-file frontmatter override.
- **Out**: cross-cycle session reuse (deliberately removed); orchestrator-style long-lived ticket session (removed).

## Dependencies

- roki-engine-iteration-loop

## Critical FR references

- fr:01-engine-model
- fr:04-phase-execution
