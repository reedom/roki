# Examples

Working samples for the three configuration files plus the per-state workflow
body files referenced from `WORKFLOW.yaml`.

| File | Purpose |
|---|---|
| [`roki.minimal.toml`](roki.minimal.toml) | Smallest `roki.toml` that boots |
| [`roki.annotated.toml`](roki.annotated.toml) | Every `roki.toml` key, annotated |
| [`WORKFLOW.minimal.yaml`](WORKFLOW.minimal.yaml) | Smallest `WORKFLOW.yaml` (admission + 1 rule) |
| [`WORKFLOW.annotated.yaml`](WORKFLOW.annotated.yaml) | Every WORKFLOW.yaml element across `rules:` / `cleanup:` / `on_failure:` |
| [`repos/bar.yaml`](repos/bar.yaml) | Per-repo override file referenced from `admission.repos[].workflow` |
| [`workflow-judge.md`](workflow-judge.md) | Sample `workflow/*.md` for a judge state |
| [`workflow-impl.md`](workflow-impl.md) | Sample `workflow/*.md` for an impl state |
| [`workflow-verdict.md`](workflow-verdict.md) | Sample `workflow/*.md` for a verdict state |

## Setup

```bash
mkdir -p ./workflow
cp docs/examples/roki.minimal.toml ./roki.toml
cp docs/examples/WORKFLOW.minimal.yaml ./WORKFLOW.yaml
# (optional) seed workflow/*.md from the samples
cp docs/examples/workflow-judge.md ./workflow/judge.md
cp docs/examples/workflow-impl.md ./workflow/impl.md
cp docs/examples/workflow-verdict.md ./workflow/verdict.md
# Validate the YAML before launch (sugar expansion + 8 validation rules):
roki workflow validate ./WORKFLOW.yaml
# Edit; pass LINEAR_API_TOKEN / LINEAR_WEBHOOK_SECRET via the environment.
roki run --config ./roki.toml
```

## File-shape conventions

- **`roki.toml`**: per-workspace daemon config; restart-only. Contains Linear
  access, webhook receiver, optional observability HTTP API, default cli
  line, paths, log destination.
- **`WORKFLOW.yaml`**: per-workspace dispatch table; restart-only in slice 8.
  Contains admission filter + repo allowlist + `rules:` / `cleanup:` /
  `on_failure:` lists. State machine model with `tasks:` sugar or canonical
  `start:` / `states:` / `terminals:`.
- **`workflow/*.md`**: per-state body; restart-only. YAML frontmatter (`cli`,
  `stall_seconds`) + Liquid template body. Every state is command-shape.

## Adding a new key / element

1. [`docs/reference/config.md`](../reference/config.md) — canonical schema.
2. [`roki.annotated.toml`](roki.annotated.toml) / [`WORKFLOW.annotated.yaml`](WORKFLOW.annotated.yaml) — annotated examples.
3. [`docs/fr/02-configuration.md`](../fr/02-configuration.md) — narrative.

## Maintenance

- CI parses the example TOML / YAML / Markdown files.
- Add new schema keys / elements to the `*.annotated.*` files.
- Keep `*.minimal.*` at the bare minimum that boots.
- The `workflow-*.md` samples are illustrative; operators are free to author
  their own naming and structure under `./workflow/`.
