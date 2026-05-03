# Reference: Public Artifacts

The **canonical reference** for the paths and required elements of the **public artifacts** that operators and downstream specs read or write.

## Artifact list

| Artifact | Path | Writer | Reader | Purpose | Used by | Requirements |
|---|---|---|---|---|---|---|
| `requirements.md` | `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/requirements.md` | spec-materialization turn (agent) | review gate / operator / future spec-sync | Per-issue acceptance criteria in EARS form | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md), [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-spec-gate Req 2, Req 3 |
| `review.md` | `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` | review turn (agent) | review gate / operator / distill phase | Per-criterion pass/fail + code references | [09-pre-pr-gate](../fr/09-pre-pr-gate.md), [10-post-merge-distill](../fr/10-post-merge-distill.md) | roki-review-gate Req 2, Req 3 |
| `distill-manifest.json` | `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/distill-manifest.json` | sweep turn (agent) | daemon validator / operator | Record of the sweep result (entries + summary + schema_version) | [10-post-merge-distill](../fr/10-post-merge-distill.md) | roki-distill-postmerge Req 7, Req 8 |
| Archive root | `<workspace_root>/<repo>/<issue>/.kiro/archive/<issue>/` | sweep turn (agent) | operator | Destination root of the `archive` disposition (mirror layout) | [10-post-merge-distill](../fr/10-post-merge-distill.md) | roki-distill-postmerge Req 10 |

## Required elements of `requirements.md`

- **File presence**: exists
- **Non-empty**: not empty
- **Encoding sanity**: encoding is sane
- **EARS shape**: at least one EARS trigger keyword (`WHEN` / `IF` / `WHILE` / `WHERE` / `SHALL`) appears at an acceptance-criteria position

Validation is performed by **mechanical regex only** (no LLM).

## Required elements of `review.md`

| Field | Type / range | Meaning |
|---|---|---|
| Overall status | `pass` or `fail` | Verdict for the whole review |
| Per-criterion entries | array | One entry for each numeric requirement ID in `requirements.md` |
| `status` of each per-criterion entry | `pass` or `fail` | Verdict for the individual criterion |
| `code_references` of each per-criterion entry (only when status=`pass`) | One or more workspace-relative file paths (optional line range) | The code positions that justify a `pass` (must be on-disk reachable at validation time) |

Daemon-side validation failure codes:

| Code | Condition |
|---|---|
| `fail-missing` | Artifact not present |
| `fail-schema` | Failed to parse against the published schema |
| `fail-evidence` | A code reference for a `pass` entry is not reachable on disk |
| `fail-timeout` | The review turn did not complete within `extension.gates.review.timeout_ms` |
| `fail-missing-spec` | `requirements.md` is missing |

## Required elements of `distill-manifest.json`

### Top-level

| Field | Type | Meaning |
|---|---|---|
| `schema_version` | string | Stable identifier. Only the values listed in `SPEC.md` are valid |
| `entries` | array of entry | Records, one per artifact |
| `summary` | object | Aggregates per disposition + the presence of execution failures |

### Entry

| Field | Type | Meaning |
|---|---|---|
| original path | workspace-relative path | Original location of the discovered artifact |
| disposition | one of `delete` / `archive` / `distill` | The disposition that was applied |
| destination path or deletion marker | path or marker | The result. For `archive`, a path under the archive root; for `distill`, a path in the stable home; for `delete`, a deletion marker |
| rule source | a rule id from `distill.routes` or the literal `default-archive` | Which rule was selected |
| timestamp | RFC 3339 | The time of processing |

### Write rules

- **Single write**: written as a complete single write (no partial / streaming writes).
- **Path containment**: nothing is written outside the workspace root + the configured project archive root. Escapes via symlink / hardlink are rejected after canonicalization.

## Archive root rule

- **Root**: `<workspace_root>/<repo>/<issue>/.kiro/archive/<issue>/`
- **Mirroring**: the original workspace-relative path is preserved / mirrored under the archive root.
- **No overwriting an existing archive**: if a destination from a prior run's recognizable manifest exists, it is authoritative.
- **Boundary**: nothing is archived outside `.kiro/archive/<issue>/` (with the exception of the project archive root declared in `distill.routes`).

## When adding a new artifact

1. Add a row to the **Artifact list** table above.
2. Add a section listing the required elements.
3. Link to this reference from the FR pages that use it.
4. Update the corresponding requirements.

## Related reference

- [config.md](config.md): a custom archive root can be declared via `extension.distill.routes`
- [extension-surface.md](extension-surface.md): the pre-cleanup vetoable hook (consumed by the distill phase)
