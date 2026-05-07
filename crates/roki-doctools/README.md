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

Cross-reference graph tooling. Reads YAML `refs:` frontmatter across the repo and answers dependency, doc-of-record, and per-kind index queries.

Kind manifest: [`docs/kinds.md`](../../docs/kinds.md).

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
