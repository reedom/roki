# Cross-Reference Schema (`refs`)

Machine-readable cross-references between roadmap, specs, FR pages, reference pages, and source modules. Every Markdown file under `.kiro/steering/`, `.kiro/specs/<spec>/`, `docs/fr/`, `docs/reference/`, and `docs/examples/` SHOULD carry a `refs:` block in its YAML front matter.

[`roki-doctools`](../../crates/roki-doctools/) consumes these blocks. Hand-edited, not generated.

Valid kinds are configured in [`docs/kinds.md`](../../docs/kinds.md) (resolved at runtime as `${ROKI_DOC_ROOT}/kinds.md`, default `ROKI_DOC_ROOT=docs`). Adding a kind = edit that manifest, no code change.

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

`kind` MUST match a kind in [`docs/kinds.md`](../../docs/kinds.md). `scope` and `sub` shape is per-kind convention (`id_pattern`); validator enforces uniqueness only.

## Kinds manifest

The full set of kinds, path globs, and INDEX generation is declared in [`docs/kinds.md`](../../docs/kinds.md):

- **Add a new kind** → edit the YAML block in `docs/kinds.md`.
- **Generate an INDEX** → add `index: { output: <path> }` to the kind's entry.
- **Remove a kind** → delete its entry; existing front matter using it will fail validation.

## Requirements

`requirements.md` carries `id: requirements:<spec>`, kind `requirements`, and enumerates child IDs in `provides`:

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

The validator does not parse the body; `provides` MUST list every requirement number a downstream artifact may reference (including sub-IDs like `req:roki-mvp:1.6`).

## Modules

`modules:` declares "this doc is the documentation of record for that source path." `touched <files>` reverses the relationship.

- No trailing slash: literal file path.
- Trailing slash: directory prefix (any file underneath).

```yaml
modules:
  - crates/roki-daemon/src/runtime.rs        # exact file
  - crates/roki-daemon/src/orchestrator/     # any file under this directory
```

A file MAY appear in multiple docs' `modules:` lists; doctools surfaces all.

## Authoring workflow

1. Pick the primary `id` matching the kind's pattern in `docs/kinds.md`.
2. List upstream artifacts this doc satisfies in `implements:`.
3. List upstream artifacts this doc would be wrong without in `depends_on:`.
4. List weak see-also links in `related:`.
5. For docs of record for code, list source paths in `modules:`.

FR pages keep their prose "Traceability" section. Front matter is the machine-readable mirror; the two MUST agree. Cross-checking is a code-review responsibility.

## Generated index files

`roki-doctools index` writes per-kind `index.md` for kinds with `index.output` set. Graph nodes (`kind: index`, `generated: true`); never hand-edit. Sibling `README.md` may carry human narrative.

`roki-doctools index map` writes `${ROKI_DOC_ROOT}/map.md`, `${ROKI_DOC_ROOT}/ai/graph.json`, and `${ROKI_DOC_ROOT}/ai/modules.md`.

Naming: only `README.md` is capitalized; all other generated/config files lowercase.

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

- Validator does not check prose `Traceability` agreement with front matter — humans audit.
- Validator does not parse `### Requirement N:` headings; `provides:` is the source of truth.
- `id_pattern` in `docs/kinds.md` is documentation, not enforcement; uniqueness is the only ID check.
- No glob expansion in `modules:` — literal paths and directory prefixes only.
- No automatic doc-to-doc propagation — schema is for findability, not generation.
