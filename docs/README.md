# roki docs

| Directory | Purpose | Entry point |
|---|---|---|
| [`fr/`](fr/) | Per-feature narrative | [fr/INDEX.md](fr/INDEX.md) |
| [`reference/`](reference/) | Lookup tables (CLI / config / artifacts / extension surface / log events) | [reference/README.md](reference/README.md) |
| [`examples/`](examples/) | Working `roki.toml` / `WORKFLOW.md` samples | [examples/README.md](examples/README.md) |

## Where to start

- Understand a feature → [fr/INDEX.md](fr/INDEX.md).
- Look up a flag / config key / log event → [reference/](reference/).
- Run it → `cp` a `*.minimal.*` file from [examples/](examples/).
- Every key → `*.annotated.*` files in [examples/](examples/).
- Implement a spec → follow the Traceability section of the FR page to `.kiro/specs/<spec>/`.

## Upstream

- Roadmap: `.kiro/steering/roadmap.md`.
- Specs: `.kiro/specs/<spec>/` (requirements.md, design.md, tasks.md).
