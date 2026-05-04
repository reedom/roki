---
refs:
  id: crate:roki-daemon
  kind: crate
  title: "roki-daemon"
  spec: roki-mvp
  implements:
    - requirements:roki-mvp
  related:
    - design:roki-mvp
  modules:
    - crates/roki-daemon/
---

# roki-daemon

The single-binary daemon for roki: observes Linear, supervises one long-lived orchestrator session (`claude --input-format stream-json --output-format stream-json`) plus zero-or-more short-lived phase subprocesses (`claude -p '/kiro-* <args>' --output-format stream-json`) per ticket, and reconciles per-issue state on restart. Owns no Linear writes, no PR creation, no code edits — Linear writes belong to the orchestrator session via the operator's installed Linear MCP; PR / git / code edits belong to phase subprocesses.

For feature-level narrative, start at [`docs/fr/index.md`](../../docs/fr/index.md). For the vertical-slice spec, see [`.kiro/specs/roki-mvp/`](../../.kiro/specs/roki-mvp/).

## Build

```sh
cargo build -p roki-daemon
```

## Run

```sh
cargo run -p roki-daemon -- run --config ./roki.toml
```

External CLIs `wt` (worktrunk) and `ghq` must be on `$PATH`; see [`docs/reference/cli.md`](../../docs/reference/cli.md) for flag-level documentation.
