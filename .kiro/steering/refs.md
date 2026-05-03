# Cross-Reference Schema (`refs`)

Machine-readable cross-references between roadmap, specs, FR pages, reference pages, and source modules. Every Markdown file under `.kiro/steering/`, `.kiro/specs/<spec>/`, `docs/fr/`, `docs/reference/`, and `docs/examples/` SHOULD carry a `refs:` block in its YAML front matter so the doc graph is mechanically traversable.

The CLI tool [`roki-doctools`](../../crates/roki-doctools/) consumes these blocks. Hand-edit them; they are not generated.

The set of valid kinds and their conventions is configured in [`docs/kinds.md`](../../docs/kinds.md) (resolved at runtime as `${ROKI_DOC_ROOT}/kinds.md`, default `ROKI_DOC_ROOT=docs`). Adding a new kind only requires editing that manifest — no code change.

## Front-matter shape

```yaml
---
refs:
  id: <kind>:<scope>[:<sub>]    # required, globally unique
  kind: <kind>                   # required, must match a name in docs/kinds.md
  title: "<free text>"           # optional, used by index / show output
  spec: <spec-name>              # optional, parent spec (null for cross-spec docs)
  provides:                      # optional, additional IDs declared inside this file
    - <id>
  implements:                    # optional, IDs this file fulfills
    - <id>
  depends_on:                    # optional, hard upstream IDs
    - <id>
  related:                       # optional, weak see-also (non-blocking)
    - <id>
  modules:                       # optional, repo-relative source paths or dir prefixes
    - <path>
    - <path-prefix>/             # trailing slash: any file under this directory
---
```

All list fields default to empty.

## Relation semantics

| Field | Meaning | Direction | Strength |
|---|---|---|---|
| `implements` | "I exist to satisfy this upstream artifact." | downstream → upstream | hard |
| `depends_on` | "I would be incorrect or incomplete without this." | downstream → upstream | hard |
| `related` | "See also." | bidirectional | soft |
| `provides` | "I declare these additional IDs inside my body." | self → child IDs | hard |
| `modules` | "I am the design of record for these source paths." | doc → code | hard |

Forward graph (`implements` + `depends_on`) is what `roki-doctools impact` traverses to answer "if I change X, what else needs review?". `related` is informational only by default; pass `--include-related` to fold it into the traversal.

## ID grammar

```
id := <kind> | <kind>:<scope> | <kind>:<scope>:<sub>
```

The `kind` prefix MUST match one of the kinds defined in [`docs/kinds.md`](../../docs/kinds.md). The validator rejects any frontmatter whose `kind:` is not in that manifest.

`scope` and `sub` shape is by convention per kind (see `id_pattern` in the manifest). The validator enforces only uniqueness, not pattern shape.

## Kinds manifest

The full set of kinds, their path globs, and which kinds get a generated INDEX file is declared in [`docs/kinds.md`](../../docs/kinds.md). When you need to:

- **Add a new kind** (e.g., `policy`, `prompt`, `adr`) → edit the YAML block in `docs/kinds.md`.
- **Have a generated INDEX for an existing kind** → add `index: { output: <path> }` to the kind's entry.
- **Remove a kind** → delete its entry; any front matter still using it will fail validation.

## How requirements work

`requirements.md` for a spec carries `id: requirements:<spec>`, kind `requirements`, and enumerates its child IDs in `provides`:

```yaml
refs:
  id: requirements:roki-mvp
  kind: requirements
  spec: roki-mvp
  provides:
    - req:roki-mvp:1
    - req:roki-mvp:2
    # ... up to the last requirement number
```

The validator does not parse the body; `provides` MUST list every requirement number a downstream artifact may reference (including any sub-IDs like `req:roki-mvp:1.6` that callers cite).

## How modules work

Listing a path in `modules:` says "this doc is the documentation of record for that source path." A `touched <files>` query reverses the relationship: given changed files, find the docs whose `modules` cover them.

- An entry without a trailing slash is matched literally as a file path.
- An entry with a trailing slash is matched as a directory prefix (any file underneath).

```yaml
modules:
  - crates/roki-daemon/src/runtime.rs        # exact file
  - crates/roki-daemon/src/orchestrator/     # any file under this directory
```

The same file MAY appear in multiple docs' `modules:` lists; doctools surfaces all of them.

## Authoring workflow

When a doc is written or edited:

1. Pick the doc's primary `id` matching its kind's pattern in `docs/kinds.md`.
2. List every upstream artifact this doc satisfies in `implements:`.
3. List every upstream artifact this doc would be wrong without in `depends_on:`.
4. List weak see-also links in `related:`.
5. For files that are the design of record for code, list source paths in `modules:`.

The natural-language "Traceability" section at the bottom of FR pages stays — humans still read prose. Front matter is the machine-readable mirror of the same information; the two MUST agree. Cross-checking is a code-review responsibility, not a validator one.

## Generated index files

`roki-doctools index` writes per-kind `index.md` files for every kind whose manifest entry has `index.output` set. These files are first-class graph nodes (`kind: index`, `generated: true`) and never hand-edited. If a human-readable narrative is wanted alongside, place it in a sibling `README.md` — the human translation of the machine index.

`roki-doctools index map` writes the global cross-kind map (`${ROKI_DOC_ROOT}/map.md`), the AI graph dump (`${ROKI_DOC_ROOT}/ai/graph.json`), and the source-to-doc index (`${ROKI_DOC_ROOT}/ai/modules.md`).

Naming convention: only `README.md` is capitalized; every other generated or config file is lowercase.

## Tooling reference

```sh
# graph integrity (CI gate)
roki-doctools validate

# editor / dev-loop queries
roki-doctools impact <id> [<id>...] [--include-related]
roki-doctools deps   <id> [<id>...] [--include-related]
roki-doctools show   <id>
roki-doctools touched <file> [<file>...]
roki-doctools list

# index regeneration (idempotent)
roki-doctools index map     # global MAP + ai/graph.json + ai/modules.md
roki-doctools index         # all per-kind INDEX files
```

## Out of scope (deliberately)

- The validator does not check that prose `Traceability` sections agree with the front matter — humans audit that.
- The validator does not parse `### Requirement N:` headings; `provides:` is the source of truth for which requirement IDs exist.
- The `id_pattern` field in `docs/kinds.md` is documentation, not enforcement; uniqueness is the only ID check.
- No glob expansion in `modules:` — only literal paths and directory prefixes.
- No automatic propagation of doc edits ("doc → doc update") — this schema is for findability, not generation.
