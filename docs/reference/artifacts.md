---
refs:
  id: ref:artifacts
  kind: reference
  title: "Public Artifacts"
---

# Reference: Public Artifacts

The **canonical reference** for the paths and required elements of the **public artifacts** that operators and downstream specs read or write.

## Artifact list

| Artifact | Path | Writer | Reader | Purpose | Used by | Requirements |
|---|---|---|---|---|---|---|
| `requirements.md` | `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/requirements.md` | spec-materialization turn (agent) | review gate / operator / future spec-sync | Per-issue acceptance criteria in EARS form | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md), [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-spec-gate Req 2, Req 3 |
| `review.md` | `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` | worker session via `kiro-review` skill (agent, before clean exit) | review gate / operator | Per-criterion pass/fail + code references | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-review-gate Req 2, Req 3 |

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
| `fail-missing-spec` | `requirements.md` is missing |

## When adding a new artifact

1. Add a row to the **Artifact list** table above.
2. Add a section listing the required elements.
3. Link to this reference from the FR pages that use it.
4. Update the corresponding requirements.

## Related reference

- [config.md](config.md): operator-facing configuration knobs
- [extension-surface.md](extension-surface.md): the orchestrator's vetoable hooks consumed by the spec gate and the review gate
