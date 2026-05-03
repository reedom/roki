# roki-doctools — Subcommand reference

Every command runs from the repo root via `cargo run -q -p roki-doctools -- <subcommand> [args]`. The `--root <DIR>` global flag overrides the working directory inference; `ROKI_DOC_ROOT` (default `docs`) overrides the directory containing `kinds.md`.

## `validate`

```sh
cargo run -q -p roki-doctools -- validate
```

CI gate. Reads every `refs:` block reachable from the kind manifest's scan roots, builds the graph, and checks integrity invariants.

**Errors emitted:**
- `front matter parse error` — YAML inside the frontmatter block did not parse.
- `unknown kind` — `kind:` value is not declared in `docs/kinds.md`.
- `duplicate id` — two `refs:` blocks declare the same `id:`, OR two `provides:` lists declare overlapping IDs.
- `dangling reference` — an `implements` / `depends_on` / `related` entry names an ID that no doc declares (neither as a primary `id:` nor in any `provides:` list).
- `module path … does not exist` — a path in `modules:` was renamed or deleted on disk.
- `matches kind … glob but has no refs: block` — a file matching a kind's `path_globs` has no frontmatter; either add `refs:` or tighten the glob.
- `bad glob` — a `path_globs` entry in `docs/kinds.md` is not a valid glob.

**Exit codes:** `0` on success (`OK (N docs)` to stdout), `1` on validation errors (one bullet per error to stderr, then `N error(s)`), `2` on internal errors.

The hook runs this after every `.md` edit. Run it manually only to confirm a fix or to bisect a hook message.

## `impact <id>... [--depth N] [--include-related]`

```sh
cargo run -q -p roki-doctools -- impact req:roki-mvp:1
cargo run -q -p roki-doctools -- impact design:roki-mvp:bootstrap --include-related
cargo run -q -p roki-doctools -- impact requirements:roki-mvp --depth 1
```

Forward closure: every doc whose graph reaches the given IDs through hard edges (`implements` / `depends_on`). Output groups results by depth so the immediate dependents come first.

**When to use:** before renaming or removing an ID, before changing the meaning of an upstream artifact, or to scope a code review's documentation impact.

**Flags:**
- `--depth N` — bound traversal at N hops (default unbounded).
- `--include-related` — also follow soft `related:` edges (printed as a separate annotation in the header).

**Output shape:**
```
Affected by changes to:
  req:roki-mvp:1

depth 1:
  fr:01-daemon-lifecycle               Daemon Lifecycle
  ref:cli                              CLI Flags
depth 2:
  ...
```

Empty closure prints `(none)`.

## `deps <id>... [--depth N] [--include-related]`

```sh
cargo run -q -p roki-doctools -- deps fr:01-daemon-lifecycle
cargo run -q -p roki-doctools -- deps tasks:roki-mvp --include-related
```

Reverse closure: every doc the given IDs depend on through hard edges. Mirror image of `impact`. Useful for understanding what context a doc presupposes before reading or editing it.

Same flag set as `impact`.

## `show <id>`

```sh
cargo run -q -p roki-doctools -- show fr:13-observability-logs
cargo run -q -p roki-doctools -- show req:roki-mvp:11
```

Prints one doc's frontmatter (id, kind, spec, title, path, every list field) plus immediate forward `reverse:` edges (who depends on this) and immediate soft `related_reverse:` edges (who mentions this in `related:`).

When given an ID that is `provides`-declared inside a parent doc (e.g. `req:roki-mvp:1`), the output also notes which parent file owns it (`queried: req:roki-mvp:1 (provided by requirements:roki-mvp)`).

**When to use:** confirming an ID resolves, checking the exact frontmatter without opening the file, finding immediate citation sites.

## `touched <file>... [--no-closure]`

```sh
cargo run -q -p roki-doctools -- touched crates/roki-daemon/src/runtime.rs
cargo run -q -p roki-doctools -- touched crates/roki-daemon/src/config/mod.rs --no-closure
```

Reverses the `modules:` relationship: given source file paths, print the docs whose `modules:` cover them (the "docs of record"), then by default also print the transitively affected docs via the forward graph.

**When to use:** after a code change, to find which docs may need updating; the hook surfaces this automatically on source edits but ad-hoc queries are fine.

**Module matching rules:**
- Exact path (no trailing slash) matches the file literally.
- Directory prefix (with trailing slash, e.g. `crates/roki-daemon/src/config/`) matches any file under that directory.

**Flags:**
- `--no-closure` — print only the docs of record, skip the transitive impact section.

## `index [target]`

```sh
cargo run -q -p roki-doctools -- index           # default: per-kind indexes
cargo run -q -p roki-doctools -- index kind      # explicit equivalent
cargo run -q -p roki-doctools -- index map       # map.md + ai/graph.json + ai/modules.md
```

Idempotent generators that write derived files. Generated files carry `kind: index`, `generated: true` frontmatter and are themselves first-class graph nodes — never hand-edit them.

**`index` (or `index kind`)** — for every kind whose manifest entry has `index.output`, write a per-kind table to that path. Currently used for `fr`, `reference`, and `crate` kinds. Output is grouped by `spec:` when the manifest entry sets `group_by: spec`.

**`index map`** — write three derived files into `${ROKI_DOC_ROOT}`:
- `map.md` — the global cross-kind map (table per kind: ID, title, spec, link).
- `ai/graph.json` — machine-readable graph dump consumed by AI tools.
- `ai/modules.md` — source-path → docs-of-record table.

**When to use:** after a bulk frontmatter edit (e.g., adding `provides:` blocks to several specs), after adding `index.output` to a kind in the manifest, or when the hook reports that index files are stale. The hook does NOT regenerate indexes.

## `list`

```sh
cargo run -q -p roki-doctools -- list
```

Print every known ID, kind, and path, one per line. Debug aid for finding the right ID to query. Pipe to `grep` for ad-hoc lookups:

```sh
cargo run -q -p roki-doctools -- list | grep req:roki-mvp
```

## Global flags

- `--root <DIR>` — repo root override; defaults to the current working directory.
- `ROKI_DOC_ROOT` (env var) — directory containing `kinds.md`; defaults to `docs`. Override is rarely correct because the hook and CI assume the default.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. |
| 1 | Validation failed (errors on stderr). |
| 2 | Internal error: bad CLI args, missing manifest, unreadable file, etc. |
