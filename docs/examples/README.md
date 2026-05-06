# Examples

Working samples for the three configuration files plus the per-phase
workflow body files referenced from `WORKFLOW.toml`.

| File | Purpose |
|---|---|
| [`roki.minimal.toml`](roki.minimal.toml) | Smallest `roki.toml` that boots |
| [`roki.annotated.toml`](roki.annotated.toml) | Every `roki.toml` key, annotated |
| [`WORKFLOW.minimal.toml`](WORKFLOW.minimal.toml) | Smallest `WORKFLOW.toml` (admission + 1 rule) |
| [`WORKFLOW.annotated.toml`](WORKFLOW.annotated.toml) | Every WORKFLOW.toml element across `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` |
| [`workflow-judge.md`](workflow-judge.md) | Sample `workflow/*.md` for a session-shape pre phase |
| [`workflow-impl.md`](workflow-impl.md) | Sample `workflow/*.md` for a command-shape run phase |
| [`workflow-verdict.md`](workflow-verdict.md) | Sample `workflow/*.md` for a session-shape post phase |

## Setup

```bash
mkdir -p ./workflow
cp docs/examples/roki.minimal.toml ./roki.toml
cp docs/examples/WORKFLOW.minimal.toml ./WORKFLOW.toml
# (optional) seed workflow/*.md from the samples
cp docs/examples/workflow-judge.md ./workflow/judge.md
cp docs/examples/workflow-impl.md ./workflow/impl.md
cp docs/examples/workflow-verdict.md ./workflow/verdict.md
# Edit; pass LINEAR_API_TOKEN / LINEAR_WEBHOOK_SECRET via the environment
roki run --config ./roki.toml
```

## File-shape conventions

- **`roki.toml`**: per-workspace daemon config; restart-only. Contains Linear
  access, webhook receiver, optional observability HTTP API, default cli
  lines, paths, log destination.
- **`WORKFLOW.toml`**: per-workspace dispatch table; hot-reloaded. Contains
  admission filter + repo allowlist + `[[rule]]` / `[[cleanup]]` /
  `[[on_failure]]` lists.
- **`workflow/*.md`**: per-phase prompt / cmd bodies; hot-reloaded. YAML
  frontmatter (`session`, `cli`, `stall_seconds`) + Liquid template body.

## Adding a new key / element

1. [`docs/reference/config.md`](../reference/config.md) — canonical schema.
2. [`roki.annotated.toml`](roki.annotated.toml) / [`WORKFLOW.annotated.toml`](WORKFLOW.annotated.toml) — annotated examples.
3. [`docs/fr/02-configuration.md`](../fr/02-configuration.md) — narrative.

## Maintenance

- CI parses the example TOML / Markdown files.
- Add new schema keys / elements to the `*.annotated.*` files.
- Keep `*.minimal.*` at the bare minimum that boots.
- The `workflow-*.md` samples are illustrative; operators are free to author
  their own naming and structure under `./workflow/`.
