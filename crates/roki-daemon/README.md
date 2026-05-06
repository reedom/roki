---
refs:
  id: crate:roki-daemon
  kind: crate
  title: "roki-daemon"
  spec: roki-skeleton
  modules:
    - crates/roki-daemon/
---

# roki-daemon

Single-binary daemon: observes Linear, supervises one long-lived orchestrator session (`claude --input-format stream-json --output-format stream-json`) plus short-lived phase subprocesses (`claude -p '/kiro-* <args>' --output-format stream-json`) per ticket, reconciles per-issue state on restart. Owns no Linear writes, no PR creation, no code edits — Linear writes belong to the orchestrator (via operator's Linear MCP); PR / git / code edits belong to phase subprocesses.

Narrative: [`docs/fr/index.md`](../../docs/fr/index.md). Specs: [`.kiro/specs/`](../../.kiro/specs/) (Wave 0 backbone: `roki-skeleton`).

## Build

```sh
cargo build -p roki-daemon
```

## Run

```sh
cargo run -p roki-daemon -- run --config ./roki.toml
```

External CLIs `wt` (worktrunk) and `ghq` must be on `$PATH`; see [`docs/reference/cli.md`](../../docs/reference/cli.md) for flag-level documentation.
