# Reference

Lookup documents for roki.

## Position in the documentation stack

| Directory | Purpose | How to read |
|---|---|---|
| [`docs/fr/`](../fr/) | Per-feature narrative | Read through |
| **`docs/reference/`** (this directory) | Exhaustive lookup tables | Look up in tables |

## Index

| File | Contents |
|---|---|
| [cli.md](cli.md) | All CLI flags of `roki run`, `roki log`, `roki events`, `roki repo` |
| [config.md](config.md) | `roki.toml` schema, `WORKFLOW.toml` schema, `workflow/*.md` frontmatter schema |
| [artifacts.md](artifacts.md) | Paths and schemas of daemon-written artifacts (`meta.json`, per-iter captures) |
| [log-events.md](log-events.md) | Canonical structured log event names |

## Update rules

- When you add a new CLI flag / config key / artifact field / extension surface / log event, add a row to the corresponding reference.
- Each entry is the **canonical home** of the definition; FR pages link here instead of restating.
- Each entry lists "Used by" (FR pages) and the corresponding requirement (`<spec> Req N.M`) for two-way traceability.
