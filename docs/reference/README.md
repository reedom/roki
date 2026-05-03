# Reference

`docs/reference/` collects the **comprehensive lookup documents** for roki.

## Position in the documentation stack

| Directory | Purpose | How to read |
|---|---|---|
| [`docs/fr/`](../fr/) | Per-feature narrative (what the feature does / why it is needed / how it behaves) | Read through |
| **`docs/reference/`** (this directory) | Exhaustive lookup tables (the meaning of this flag / key / event) | Look up in tables |

reference is documentation that **operators look up at runtime**. Its readers and timing differ from the narrative.
It prioritizes completeness and at-a-glance access; it does not carry a story.

## Index

| File | Contents |
|---|---|
| [cli.md](cli.md) | All CLI flags of `roki run` |
| [config.md](config.md) | `roki.toml` schema, `WORKFLOW.md` schema (including reserved extension namespaces) |
| [artifacts.md](artifacts.md) | Paths and required elements of public artifacts (`requirements.md` / `review.md` / `distill-manifest.json`) |
| [extension-surface.md](extension-surface.md) | Traits / hooks / context channels that downstream specs depend on |
| [log-events.md](log-events.md) | The list of structured log events |

## Update rules

- **When you add a new CLI flag / config key / artifact field / extension surface / log event, add a row to the corresponding reference.**
- Each entry in reference is the **canonical home** of the definition. The FR side does not restate it; it links here.
- Each entry should also list "which FR page uses it" and "the corresponding requirement", so that traceability runs both ways.

## Traceability

- Each entry → FR pages: linked in the "Used by" column.
- Each entry → requirements: in the form `<spec> Req N.M`.
