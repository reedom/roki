---
name: roki-doctools
description: This skill should be used when the user asks to "validate the doc graph", "regenerate the index", "find docs that depend on X", "find what depends on Y", "show frontmatter for ID Z", "find docs of record for a source file", "rebuild map.md", or invokes any roki-doctools subcommand (validate, impact, deps, show, touched, index, list). Also use proactively when adding, renaming, or removing a `refs:` ID, when the post-edit hook reports a doctools failure, or when the user mentions dangling references, duplicate IDs, frontmatter cross-references, or `docs/kinds.md`. Comprehensive reference for the roki-doctools CLI in `crates/roki-doctools/`.
---

# roki-doctools

## Purpose

`roki-doctools` is the cross-reference graph CLI for this repository. It reads YAML `refs:` frontmatter from every Markdown file under the configured doc roots (steering, specs, fr, reference, examples, crate READMEs) and answers questions about a directed graph whose nodes are docs and whose edges are `implements` / `depends_on` / `related` / `provides` / `modules`. Use it to validate graph integrity, query impact and dependencies, locate the doc of record for a source file, and regenerate per-kind indexes.

The schema this tool consumes is documented in `docs/reference/frontmatter.md`. The kind manifest (which kinds exist, where their files live, which kinds have a generated index) is `docs/kinds.md`. The CLI source is `crates/roki-doctools/src/main.rs`.

## When to invoke this tool

A post-edit hook already runs `validate` after every `.md` edit and `touched <file>` after every source / spec / steering / `kinds.md` edit. **Do not re-run those commands manually unless investigating a specific failure.** Reach for the CLI when one of these applies:

- A hook surfaced an error and root cause needs investigation.
- Renaming or deleting an ID, kind, or doc — references elsewhere become dangling and must be updated.
- Auditing what is affected by a contemplated change before making it (`impact`, `deps`).
- Inspecting a single doc's frontmatter without opening the file (`show`).
- Mapping a source path to the doc of record without grepping (`touched`).
- Regenerating indexes after adding `index.output` to a kind, or after a bulk frontmatter edit (`index`, `index map`).
- Listing every known ID for ad-hoc searches (`list`).

## Subcommand quick reference

| Command | Purpose | Reads / writes |
|---|---|---|
| `validate` | CI gate. Detects dangling refs, duplicate IDs, unknown kinds, missing `modules:` paths, files that match a kind's glob but lack a `refs:` block. | Read-only |
| `impact <id>...` | Forward closure: every doc transitively affected if these IDs change. | Read-only |
| `deps <id>...` | Reverse closure: every doc the given IDs transitively depend on. | Read-only |
| `show <id>` | One doc's frontmatter plus immediate forward and reverse refs. | Read-only |
| `touched <file>...` | Docs of record for source files (via `modules:`) plus impact closure. | Read-only |
| `index` | Regenerate every per-kind `index.md` whose manifest entry has `index.output`. | Writes index files |
| `index map` | Regenerate `${ROKI_DOC_ROOT}/map.md`, `ai/graph.json`, `ai/modules.md`. | Writes map files |
| `list` | Print every known ID (debug aid). | Read-only |

Both `impact` and `deps` accept `--include-related` to fold in soft `related:` edges (default is hard edges only) and `--depth N` to bound traversal depth. `touched` accepts `--no-closure` to skip the transitive expansion.

Detailed per-command usage with output samples is in **`references/subcommands.md`**.

## Mental model

A `refs:` block declares one node and a set of typed edges:

```yaml
---
refs:
  id: <kind>:<scope>[:<sub>]   # required, globally unique
  kind: <kind>                  # required, must appear in docs/kinds.md
  title: "..."
  spec: <spec-name>             # optional
  provides: [<id>...]           # IDs declared inside this file (e.g. req:roki-mvp:1)
  implements: [<id>...]         # hard upstream — "I exist to satisfy this"
  depends_on: [<id>...]         # hard upstream — "I would be wrong without this"
  related: [<id>...]            # soft see-also (bidirectional, non-blocking)
  modules: [<path>, <path>/]    # repo-relative source paths or directory prefixes
---
```

Edge semantics in one line each:
- **`implements` / `depends_on`** — hard forward edges. `impact` and `deps` traverse these by default.
- **`related`** — soft. Folded in only with `--include-related`.
- **`provides`** — declares additional IDs (e.g. requirement bullets) inside a parent doc. The CLI treats both the parent's `id` and every `provides:` entry as resolvable IDs that point back to the same file.
- **`modules`** — claims source paths. Trailing slash means directory prefix (any file under it); no slash is an exact file path. `touched` reverses this lookup.

Field-level reference is in **`references/frontmatter.md`**.

## Common workflows

**A hook reported an error.** Read the message; it names the file plus the specific problem. Then investigate without re-running the hook:
- Dangling reference → grep the offending ID in the codebase, decide whether to add a missing `provides:` entry on the upstream doc, or fix the citation, or remove a stale link.
- Duplicate ID → two docs claim the same `id` or `provides` entry. One must change or move.
- Unknown kind → either a typo in `kind:`, or a new kind to register in `docs/kinds.md`.
- Missing module path → the source file in `modules:` was renamed or deleted; update the path.
- "matches kind glob but has no `refs:` block" → either add a `refs:` block to the matched file, or tighten the kind's `path_globs` in `docs/kinds.md` to exclude it.

**Renaming or deleting an ID.** Before the rename: `cargo run -q -p roki-doctools -- impact <old-id>` to see every doc that cites it. After the rename: re-run validate (the hook does this on next edit) and update the listed docs.

**Auditing source-to-doc coverage.** `cargo run -q -p roki-doctools -- touched crates/roki-daemon/src/foo.rs` returns the docs that claim that path. Use this when a code change might have public-surface implications and the right doc to update is unknown.

**Adding a numbered requirement.** When introducing `req:<spec>:N` that other docs may cite, add the new ID to the upstream `requirements.md`'s `provides:` list. The validator only catches dangling references at the citation site, not missing entries on the producer.

**Adding a new kind.** Edit the YAML block in `docs/kinds.md`. To get a generated index for that kind, add `index: { output: <path> }`. Then run `cargo run -q -p roki-doctools -- index` to materialize it.

**Bulk frontmatter edit followed by index regeneration.** After updating many `implements:` lists, run both `index` (per-kind) and `index map` (global map + graph.json + modules.md) so generated files stay current. The hook does not regenerate indexes.

End-to-end worked examples (creating a new fr doc, renaming a spec, adding a new kind) are in **`references/workflows.md`**.

## Hook integration

The post-edit hook runs:
- `roki-doctools validate` after any `.md` edit.
- `roki-doctools touched <file>` after any source / spec / steering / `kinds.md` edit, surfacing the docs of record so the operator can decide whether the public surface changed.

Hook output appears as additional context in the user's session. Do not duplicate the hook's work. Re-run `validate` manually only when the hook's message is being actively diagnosed.

## Invocation

From the repo root:

```sh
cargo run -q -p roki-doctools -- <subcommand> [args]
```

The `-q` flag suppresses cargo's compile chatter so the CLI's own output is the only thing on stdout. The first run after a clean checkout takes a few seconds while cargo compiles the crate; subsequent runs are near-instant.

Configuration:
- `ROKI_DOC_ROOT` (env var, default `docs`) — directory containing `kinds.md` and where `map.md` / `ai/graph.json` are written. Override only for experimental relocations; the repo's hook expects the default.
- `--root <DIR>` (global flag) — repo root override. Default is the current working directory.

Exit codes:
- `0` — success.
- `1` — validation failed (errors printed to stderr).
- `2` — internal error (bad arguments, IO failure, missing manifest).

## Boundaries

The validator is mechanical. It enforces:
- Frontmatter parses as YAML.
- `kind:` appears in `docs/kinds.md`.
- Every cited ID in `implements` / `depends_on` / `related` resolves to a known node.
- IDs are globally unique.
- Every path in `modules:` exists on disk.
- Every file matching a kind's `path_globs` carries a `refs:` block.

It does NOT enforce:
- That prose `## Traceability` sections agree with the frontmatter (humans audit that).
- That `provides:` enumerates every requirement actually written in the doc body (that is an authoring responsibility).
- ID-pattern shape (only uniqueness is checked).
- Glob expansion in `modules:` — only literal paths and directory prefixes are supported.

Common errors and fixes are catalogued in **`references/troubleshooting.md`**.

## Additional Resources

### Reference Files

- **`references/subcommands.md`** — detailed per-command usage with flag tables and output samples.
- **`references/frontmatter.md`** — `refs:` schema cheatsheet with field semantics, ID grammar, and examples.
- **`references/workflows.md`** — end-to-end recipes: new fr doc, rename spec, add kind, bulk migration.
- **`references/troubleshooting.md`** — error message → root cause → fix.

### Authoritative sources

- **`.claude/rules/refs.md`** — judgment-call rule auto-surfaced by the post-edit hook on the four triggers (new `.md`, `requirements.md` edit, source change, `kinds.md` edit). Read first when the hook output asks for a judgment call.
- **`docs/kinds.md`** — kind manifest (path globs, id patterns, index outputs).
- **`docs/reference/frontmatter.md`** — frontmatter schema and field reference.
- **`crates/roki-doctools/README.md`** — short crate-level usage summary.
- **`crates/roki-doctools/src/main.rs`** — implementation; consult to confirm exact behavior when documentation is ambiguous.
- **`.claude/hooks/refs-postedit.sh`** — the post-edit hook itself; consult when surprised by what the hook does or does not surface.
