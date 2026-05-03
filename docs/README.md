# roki docs

Documentation for the roki project, split into directories by purpose.

| Directory | Purpose | How to read | Entry point |
|---|---|---|---|
| [`fr/`](fr/) | **Per-feature narrative** (what the feature does / why it is needed / how it behaves) | Read through | [fr/INDEX.md](fr/INDEX.md) |
| [`reference/`](reference/) | **Comprehensive lookup tables** (CLI / config / artifacts / extension surface / log events) | Look up in tables | [reference/README.md](reference/README.md) |
| [`examples/`](examples/) | **Working configuration samples** (minimal + annotated `roki.toml` / `WORKFLOW.md`) | Copy and use | [examples/README.md](examples/README.md) |

## Where to start

- **"I want to understand roki overall"** → start from [fr/INDEX.md](fr/INDEX.md), pick a feature you care about, and read its FR page through.
- **"What does `--debug` mean?" / "What is the default of `extension.gates.spec.timeout_ms`?"** → look it up in [reference/](reference/).
- **"I just want to run it"** → `cp` one of the `*.minimal.*` files in [examples/](examples/) and pipe in environment variables.
- **"I want to know every key"** → read the `*.annotated.*` files in [examples/](examples/).
- **"I want to start implementing"** → from the Traceability section of the relevant FR page, follow the link to the corresponding `.kiro/specs/<spec>/` (requirements / design / tasks).

## Related upstream documents

- **Roadmap**: `.kiro/steering/roadmap.md` — project-wide scope and the list of specs.
- **Specs**: `.kiro/specs/<spec>/` — EARS Acceptance Criteria (requirements.md), design (design.md), and implementation tasks (tasks.md).
