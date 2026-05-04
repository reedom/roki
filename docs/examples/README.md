# Examples

| File | Purpose |
|---|---|
| [`roki.minimal.toml`](roki.minimal.toml) | Smallest config that boots |
| [`roki.annotated.toml`](roki.annotated.toml) | Every key, annotated |
| [`WORKFLOW.minimal.md`](WORKFLOW.minimal.md) | Smallest working `WORKFLOW.md` |
| [`WORKFLOW.annotated.md`](WORKFLOW.annotated.md) | Every reserved extension namespace and template variable |

## Setup

```bash
cp docs/examples/roki.minimal.toml ./roki.toml
cp docs/examples/WORKFLOW.minimal.md ./WORKFLOW.md
# edit, pass LINEAR_API_TOKEN etc. via environment
roki run --config ./roki.toml
```

## Adding a key / extension

1. [`docs/reference/config.md`](../reference/config.md) — canonical schema.
2. [`roki.annotated.toml`](roki.annotated.toml) / [`WORKFLOW.annotated.md`](WORKFLOW.annotated.md) — annotated examples.
3. [`docs/fr/02-configuration.md`](../fr/02-configuration.md) — narrative.

## Maintenance

- CI validates parseability.
- Add new schema keys / namespaces to the `*.annotated.*` files.
- Keep `*.minimal.*` at the bare minimum that boots.
