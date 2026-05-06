---
refs:
  id: brief:roki-config-per-repo-toml
  kind: brief
  title: "roki-config-per-repo-toml Brief"
  spec: roki-config-per-repo-toml
---

# Brief: roki-config-per-repo-toml

## Problem

A single workspace `WORKFLOW.toml` scales poorly across repos with divergent rule sets.

## Desired Outcome

Per design §3.3: `[[admission.repos]] workflow = "repos/<repo>.toml"` replaces the top-level `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` for that repo. Top-level admission stays in `WORKFLOW.toml`. The two rule sets do not merge.

## Scope

- **In**: per-repo TOML loader; admission → repo → workflow path lookup; replacement (no merge) semantics.
- **Out**: cross-repo rule sharing / inheritance.

## Dependencies

- roki-config-workflow-toml-full

## Critical FR references

- fr:02-configuration
