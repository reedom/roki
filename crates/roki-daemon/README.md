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

The single-binary daemon for roki: observes Linear, runs the setup judge, supervises bounded `claude` worker subprocesses, and reconciles per-issue state on restart. Owns no Linear writes, no PR creation, no code edits — those belong to the agent inside the worker subprocess.

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
