# roki-doctools — End-to-end workflows

Recipes for the most common authoring tasks. Each workflow assumes the post-edit hook is active; manual `validate` / `touched` calls are noted only where they add value beyond the hook.

## 1. Add a new fr doc

1. Pick the next number: `ls docs/fr/[0-9]*.md | sort` and choose `NN-slug.md`.
2. Look up the kind's path glob in `docs/kinds.md` — `docs/fr/[0-9]*.md` is already covered, so no manifest edit is needed.
3. Mirror the closest existing fr doc's frontmatter shape. The minimum is:
   ```yaml
   ---
   refs:
     id: fr:NN-slug
     kind: fr
     title: "Human-readable title"
   ---
   ```
4. Add `depends_on:`, `related:`, `modules:` as needed.
5. Save the file. The hook runs `validate`; fix any errors it surfaces.
6. Run `cargo run -q -p roki-doctools -- index` to regenerate `docs/fr/index.md` so the new doc appears in the per-kind table.

## 2. Rename or remove an ID

1. Inventory citations: `cargo run -q -p roki-doctools -- impact <old-id>`. The output lists every doc that depends on it.
2. Rename in the producer first:
   - For a primary `id:`, edit the file's frontmatter.
   - For a `provides:` entry, edit the parent doc.
3. Update every doc the impact query listed (search-and-replace `<old-id>` → `<new-id>` or remove the citation if dropping the relationship).
4. Save. The hook re-validates; investigate any remaining dangling references.

For a deletion, follow the same flow but remove the citation entirely instead of replacing it.

## 3. Rename a source file referenced in `modules:`

1. Find the docs of record for the old path: `cargo run -q -p roki-doctools -- touched <old/path>`.
2. Rename the file in source control.
3. Update each doc's `modules:` to the new path.
4. The hook validates — `validate` will report `module path ... does not exist` for any miss.

## 4. Add a new kind

1. Edit the YAML block in `docs/kinds.md`:
   ```yaml
   - name: <new-kind>
     path_globs: ["<glob>"]
     id_pattern: "<new-kind>:{slug}"
     index:                      # optional — only if a generated index is wanted
       output: <path/to/index.md>
   ```
2. Save `docs/kinds.md`. The hook validates the manifest; failures point at malformed YAML or duplicate kind names.
3. Add `refs:` blocks with `kind: <new-kind>` to existing files that match the new glob, OR tighten the glob if some files should remain outside the graph. Validation will fail with `matches kind ... glob but has no refs: block` until every matching file is covered or excluded.
4. If `index.output` was set, run `cargo run -q -p roki-doctools -- index` to materialize the file.

## 5. Bulk frontmatter edit

When editing many docs at once (e.g., backfilling `provides:` across multiple specs):

1. Make all the edits.
2. Run `cargo run -q -p roki-doctools -- validate` once at the end to confirm graph integrity.
3. Run `cargo run -q -p roki-doctools -- index` to regenerate per-kind indexes.
4. Run `cargo run -q -p roki-doctools -- index map` to refresh `map.md`, `ai/graph.json`, and `ai/modules.md`.
5. Commit all the modified files together.

## 6. Investigate "what does this code touch in docs?"

```sh
cargo run -q -p roki-doctools -- touched crates/roki-daemon/src/runtime.rs
```

Output names every doc whose `modules:` covers the file (docs of record) plus the transitively affected docs through the forward graph. Use the result to decide which docs need updating before opening a PR.

For multiple files at once: `touched <file1> <file2> ...`. The closure section is computed against all files together.

## 7. Investigate "what depends on this concept?"

```sh
cargo run -q -p roki-doctools -- impact ref:cli
cargo run -q -p roki-doctools -- impact ref:cli --include-related
cargo run -q -p roki-doctools -- impact crate:roki-doctools --depth 1
```

`--include-related` folds in soft `related:` edges (default is hard edges only). `--depth N` bounds traversal at N hops, useful for very wide closures.

## 8. Investigate "what does this doc presuppose?"

```sh
cargo run -q -p roki-doctools -- deps fr:13-observability-logs
cargo run -q -p roki-doctools -- deps fr:01-daemon-lifecycle --include-related
```

Reverse traversal — every doc the given IDs eventually depend on. Use to onboard onto a doc by reading its hard upstream dependencies first.

## 9. Quickly inspect a doc

```sh
cargo run -q -p roki-doctools -- show fr:01-daemon-lifecycle
cargo run -q -p roki-doctools -- show ref:cli
```

Prints frontmatter plus immediate forward-reverse and `related:`-reverse edges. Cheaper than opening the file when only the metadata is needed.

## 10. Generate / refresh derived files

| Want to refresh | Command |
|---|---|
| Per-kind indexes (`docs/fr/index.md`, `docs/reference/index.md`, `crates/index.md`) | `cargo run -q -p roki-doctools -- index` |
| Global map (`docs/map.md`) and AI graph (`docs/ai/graph.json`, `docs/ai/modules.md`) | `cargo run -q -p roki-doctools -- index map` |
| Both | run them in sequence |

These are idempotent. Re-running on an unchanged graph produces byte-identical output.
