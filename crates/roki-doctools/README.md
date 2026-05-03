---
refs:
  id: crate:roki-doctools
  kind: crate
  title: "roki-doctools"
  related:
    - ref:cli
  modules:
    - crates/roki-doctools/
---

# roki-doctools

Cross-reference graph tooling for roki specs and docs. Reads YAML `refs:` frontmatter across the repository and answers "what depends on what / who is the design of record for this source path / regenerate the per-kind indexes."

Schema and conventions are documented in [`.kiro/steering/refs.md`](../../.kiro/steering/refs.md). The kind manifest (which kinds exist, where their files live, which kinds get a generated index) is at [`docs/kinds.md`](../../docs/kinds.md).

## Subcommands

```sh
# Graph integrity (CI gate)
cargo run -p roki-doctools -- validate

# Editor / dev-loop queries
cargo run -p roki-doctools -- impact <id> [<id>...] [--include-related]
cargo run -p roki-doctools -- deps   <id> [<id>...] [--include-related]
cargo run -p roki-doctools -- show   <id>
cargo run -p roki-doctools -- touched <file> [<file>...]
cargo run -p roki-doctools -- list

# Index regeneration (idempotent)
cargo run -p roki-doctools -- index map     # global map.md + ai/graph.json + ai/modules.md
cargo run -p roki-doctools -- index         # all per-kind index.md files
```

## Configuration

`ROKI_DOC_ROOT` (default `docs`) points at the directory containing `kinds.md` and where `map.md` / `ai/graph.json` are written.
