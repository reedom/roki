---
refs:
  id: fr:17-doc-cross-references
  kind: fr
  title: "Doc Cross-References"
  related:
    - crate:roki-doctools
    - ref:frontmatter
  modules:
    - crates/roki-doctools/
    - .kiro/steering/refs.md
    - docs/kinds.md
---

# FR 17: Doc Cross-References

> A machine-readable cross-reference graph over roadmap, specs, FR pages, reference pages, source modules, and crate READMEs — so AI agents and contributors can answer "if I change X, what else needs review?" without reading every file.

## Purpose

As the repository grows past the point where every doc fits in one mental model, "which design covers this code? which FR explains this requirement? what depends on the file I am about to edit?" become real navigation costs. This feature gives those questions a single answer surface: every doc carries a YAML `refs:` block declaring its identity, its upstream artifacts, and the source paths it documents; tooling traverses the resulting graph.

The same surface doubles as RAG / vector-search fuel for AI coding agents: a generated `ai/graph.json` and a per-source-path map (`ai/modules.md`) let an agent jump directly from a code path to the docs of record without grepping prose for `[link](...)`.

## User-visible Behavior

The contributor's editor / dev-loop flow:

- **Author or edit a doc.** Add a `refs:` front-matter block declaring `id`, `kind`, and any `implements` / `depends_on` / `related` / `modules` references.
- **Run `roki-doctools validate`.** Dangling references, duplicate IDs, unknown kinds, or missing `modules:` paths fail with a non-zero exit and a per-error line. Clean repos print `OK (N docs)`.
- **Ask "what does this change affect?"** `roki-doctools impact <id>...` prints the transitive closure of docs whose `implements` / `depends_on` chain leads back to the input. `--include-related` folds soft `related:` edges in.
- **Ask "what does this depend on?"** `roki-doctools deps <id>...` walks the same graph in reverse.
- **Ask "what is this doc?"** `roki-doctools show <id>` prints front matter plus immediate forward (hard) and reverse (related) references.
- **Map source change to docs of record.** `roki-doctools touched <file>...` reports every doc whose `modules:` covers the given files, plus the transitive impact closure.
- **Regenerate index artifacts.** `roki-doctools index` writes per-kind `index.md` for every kind whose manifest entry has `index.output` set. `roki-doctools index map` writes the global `${ROKI_DOC_ROOT}/map.md`, the AI graph dump `${ROKI_DOC_ROOT}/ai/graph.json`, and the source-to-doc index `${ROKI_DOC_ROOT}/ai/modules.md`. Both are idempotent.

The contributor never edits an `index.md` or `map.md` by hand — those carry `kind: index, generated: true` front matter and are overwritten on every `index` run.

## Capabilities

- **Manifest-driven kinds**: the set of valid kinds, their path globs, ID patterns, and which kinds get a generated index live in [`docs/kinds.md`](../kinds.md). New kinds (`policy`, `prompt`, `adr`, …) are added by editing that file alone — no code change.
- **Manifest-driven scan plan**: `roki-doctools` derives every scan root from kind `path_globs`; the CLI is repo-agnostic (point `ROKI_DOC_ROOT` at any project with a `kinds.md`).
- **Hard vs soft edges**: `implements` / `depends_on` / `provides` are hard (graph-traversal). `related:` is soft (informational; opt-in via `--include-related`).
- **Module-of-record mapping**: `modules:` entries are exact paths or directory prefixes (trailing `/`), giving each source path a known doc.
- **Reproducible AI artifact**: `ai/graph.json` is a self-contained dump of every doc + its edges + module map, suitable for vector-store ingestion, regenerated on demand.

## Boundaries

- **The validator does not parse prose** — natural-language `Traceability` sections are humans' responsibility to keep in sync with `refs:` front matter.
- **No automatic doc-to-doc propagation**: this feature is for findability, not for rewriting downstream docs when an upstream changes.
- **No glob expansion in `modules:`** — only literal paths and directory prefixes. (Glob expansion in kind `path_globs` is supported.)
- **`docs/examples/*.toml` and other non-Markdown** cannot carry front matter; non-`.md` files cannot become graph nodes today.
- **README is human-curated.** Generated `index.md` is the machine-readable source of truth; if a reader-friendly narrative is desired, place it in a sibling `README.md`.
- **Notion / external doc systems** are out of scope (this feature touches in-repo Markdown only).
- **Vector-store ingestion is operator-driven** — `ai/graph.json` is produced; loading it into Pinecone / Weaviate / Notion / Cody / etc. is left to the operator's RAG pipeline.

## Traceability

- **Schema and conventions**: [`.kiro/steering/refs.md`](../../.kiro/steering/refs.md).
- **Kind manifest**: [`docs/kinds.md`](../kinds.md).
- **Implementation**: [`crate:roki-doctools`](../../crates/roki-doctools/README.md).
- **Generated artifacts**: [`docs/map.md`](../map.md), [`docs/ai/graph.json`](../ai/graph.json), [`docs/ai/modules.md`](../ai/modules.md), per-kind `docs/<kind>/index.md`, `crates/index.md`.
- **Related FR**: none yet (this feature is meta-tooling for the doc layer).
