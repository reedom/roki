# Examples

**Working samples** of roki configuration files.

## Layout

| File | Purpose |
|---|---|
| [`roki.minimal.toml`](roki.minimal.toml) | The smallest configuration that boots. Usable as a starting point with `cp` |
| [`roki.annotated.toml`](roki.annotated.toml) | An annotated reference that lists **every key** with comments |
| [`WORKFLOW.minimal.md`](WORKFLOW.minimal.md) | The smallest working `WORKFLOW.md` (two template blocks + empty front matter) |
| [`WORKFLOW.annotated.md`](WORKFLOW.annotated.md) | An exhaustive version including every reserved extension namespace and every template variable |

## Usage

### Setting up a new workspace

```bash
cp docs/examples/roki.minimal.toml ./roki.toml
cp docs/examples/WORKFLOW.minimal.md ./WORKFLOW.md
# edit, and pass LINEAR_API_TOKEN etc. via environment variables
roki run --config ./roki.toml
```

### When adding a key / extension

To learn the full set of keys and the meaning of each, the convenient order is:

1. [`docs/reference/config.md`](../reference/config.md) — the schema tables (canonical)
2. [`roki.annotated.toml`](roki.annotated.toml) / [`WORKFLOW.annotated.md`](WORKFLOW.annotated.md) — working annotated examples
3. [`docs/fr/02-configuration.md`](../fr/02-configuration.md) — the narrative

## Maintenance policy

- These examples are placed as **real files** and their parseability is validated in CI.
- When a new key / namespace is added to the schema, also add an entry to the `*.annotated.*` files.
- Keep "minimal" at the absolute bare minimum that still boots; do not let it grow casually.
