---
refs:
  id: ref:frontmatter
  kind: reference
  title: "Frontmatter Schema"
---

# Reference: Frontmatter Schema

The YAML `refs:` block carried by every doc in the cross-reference graph.

Semantics and authoring guidance: [`.kiro/steering/refs.md`](../../.kiro/steering/refs.md). Valid kinds and on-disk locations: [`docs/kinds.md`](../kinds.md).

## Block shape

```yaml
---
refs:
  id: <kind>:<scope>[:<sub>]
  kind: <kind>
  title: "<free text>"
  spec: <spec-name>
  provides:
    - <id>
  implements:
    - <id>
  depends_on:
    - <id>
  related:
    - <id>
  modules:
    - <repo-relative path>
    - <repo-relative path-prefix>/
  generated: false
  indexes_kind: <kind-name>
---
```

Standard Markdown front matter, top of file between `---` lines.

## Fields

| Field | Type | Required | Purpose |
|---|---|---|---|
| `id` | string | yes | Globally unique identifier. Format: `<kind>` / `<kind>:<scope>` / `<kind>:<scope>:<sub>`. |
| `kind` | string | yes | Must match a `name` in [`docs/kinds.md`](../kinds.md). Unknown kinds fail validation. |
| `title` | string | no | Free-text label printed by `roki-doctools show` / `index` / `map`. |
| `spec` | string | no | Parent spec name (`roki-mvp`, `roki-spec-gate`, …). Use for kinds scoped to a spec. |
| `provides` | string list | no | Additional IDs declared inside this file's body (e.g. `requirements.md` lists every `req:<spec>:N`). |
| `implements` | string list | no | Hard upstream — IDs this doc fulfills. Walked by `impact` / `deps`. |
| `depends_on` | string list | no | Hard upstream — IDs this doc would be incorrect or incomplete without. Walked by `impact` / `deps`. |
| `related` | string list | no | Soft see-also links. Excluded from `impact` / `deps` by default; include with `--include-related`. |
| `modules` | string list | no | Repo-relative source paths or directory prefixes that this doc is the design of record for. See **Module patterns** below. |
| `generated` | bool | no | `true` for files written by `roki-doctools index` / `index map`. Excluded from `map.md` and per-kind `index.md` listings. Default: `false`. |
| `indexes_kind` | string | no | For `kind: index` files only — the kind name being indexed (round-tripped from generation). |

All list fields default to empty.

## Relation semantics

| Field | Direction | Strength | Walked by `impact` / `deps`? |
|---|---|---|---|
| `implements` | downstream → upstream | hard | yes |
| `depends_on` | downstream → upstream | hard | yes |
| `provides` | self → child IDs | hard | resolution only (children become valid targets) |
| `related` | bidirectional | soft | only with `--include-related` |
| `modules` | doc → code | hard | reverse via `touched` |

## ID grammar

```
id := <kind> | <kind>:<scope> | <kind>:<scope>:<sub>
```

Per-kind conventions in [`docs/kinds.md`](../kinds.md) (`id_pattern` is informational). Validation enforces uniqueness, not pattern shape.

| Kind | Example IDs |
|---|---|
| `roadmap` | `roadmap` |
| `brief` | `brief:roki-mvp` |
| `requirements` | `requirements:roki-mvp` |
| `req` | `req:roki-mvp:1`, `req:roki-mvp:1.6` |
| `design` | `design:roki-mvp`, `design:roki-mvp:bootstrap` |
| `tasks` | `tasks:roki-mvp` |
| `research` | `research:roki-mvp` |
| `fr` | `fr:12-daemon-lifecycle` |
| `reference` | `ref:cli`, `ref:config`, `ref:frontmatter` |
| `example` | `example:roki.minimal` |
| `crate` | `crate:roki-daemon`, `crate:roki-doctools` |
| `index` | `index:fr`, `index:reference`, `index:map`, `index:modules` (generated) |

## Module patterns

| Pattern | Matches |
|---|---|
| `crates/roki-daemon/src/runtime.rs` | exact file |
| `crates/roki-daemon/src/orchestrator/` | any file under `crates/roki-daemon/src/orchestrator/` (trailing slash required) |
| `crates/roki-daemon/src/orchestrator` | exact match for a file with that name (no trailing slash) |

A source path MAY appear in multiple docs' `modules:` lists; `roki-doctools touched <file>` surfaces all of them.

## When validation fails

`roki-doctools validate` exits non-zero on any of:

- Unknown `kind:` (not declared in `docs/kinds.md`).
- Duplicate `id` across two docs, or duplicate `provides` entry.
- Dangling reference in `implements` / `depends_on` / `related` (target ID not declared anywhere).
- A `modules:` path that does not exist on disk (file or directory).
- Front-matter YAML that fails to parse.

## See also

- [`.kiro/steering/refs.md`](../../.kiro/steering/refs.md) — full schema, authoring workflow, design rationale.
- [`docs/kinds.md`](../kinds.md) — kind manifest (paths, ID patterns, index outputs).
- [17-doc-cross-references](../fr/17-doc-cross-references.md) — feature narrative.
- [`crate:roki-doctools`](../../crates/roki-doctools/README.md) — CLI reference.
