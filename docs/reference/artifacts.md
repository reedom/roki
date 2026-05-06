---
refs:
  id: ref:artifacts
  kind: reference
  title: "Public Artifacts"
---

# Reference: Public Artifacts

Paths and required elements of public artifacts that operators and downstream specs read or write.

## Artifact list

| Artifact | Path | Writer | Reader | Purpose | Used by | Requirements |
|---|---|---|---|---|---|---|
| `meta.json` | `<session_root>/<ticket-id>/cycle-<uuid>/meta.json` | roki daemon (cycle engine) | `roki log --meta`, HTTP API `GET /api/tickets/{id}/cycles`, [09-log-access-cli](../fr/09-log-access-cli.md) | Per-cycle summary durably co-located with per-iter captures so cycle history is queryable independent of structured event log retention | [09-log-access-cli](../fr/09-log-access-cli.md), [10-http-api](../fr/10-http-api.md) | TBD |
| `requirements.md` | `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/requirements.md` | `materialize_spec` phase subprocess (per [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md) phase catalog), driven by `kiro-discovery` | the orchestrator session (structural validation per [19-orchestrator-session](../fr/19-orchestrator-session.md) §Artifact validation) / operator / future spec-sync | Per-issue acceptance criteria in EARS form | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 5.6 |
| `review.md` | `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` | `finalize_review` phase subprocess (per [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md) phase catalog), synthesizing from prior-phase verdicts (per-task `kiro-review` APPROVED set, `kiro-validate-impl` GO, `kiro-verify-completion` VERIFIED stamps, worktree artefacts) before the orchestrator's `action=stop` | the orchestrator session (structural validation per [19-orchestrator-session](../fr/19-orchestrator-session.md) §Artifact validation) / operator | Per-criterion pass/fail + code references | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 5.6 |

## Schema of `meta.json`

One JSON object per file. UTF-8, no trailing newline required.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `cycle_id` | string (UUID v4) | yes | Cycle identifier matching the parent directory `cycle-<uuid>/` |
| `ticket_id` | string | yes | Linear issue identifier the cycle belongs to |
| `repo` | string | yes | Admission-resolved ghq path (e.g. `github.com/foo/bar`) |
| `cycle_kind` | enum | yes | `rule` / `cleanup` / `failure` |
| `cycle_trigger` | enum | yes | `webhook` / `cold_start` (extensible) |
| `failed_cycle_id` | string (UUID v4) or null | when `cycle_kind == "failure"`, otherwise null | UUID of the cycle this failure handler is recovering |
| `started_at` | RFC 3339 timestamp | yes | UTC; matches the `cycle_started` event timestamp |
| `ended_at` | RFC 3339 timestamp or null | yes | Null while the cycle is in flight |
| `iter_count` | int | yes | Number of completed iterations |
| `terminal_directive` | enum or null | one of `terminal_directive` / `failure_kind` is non-null when `ended_at` is set | `run` / `end` / `pre` (the post directive that terminated the cycle), or null if the cycle ended through a failure |
| `failure_kind` | enum or null | see above | `process_crash` / `unparseable` / `schema_drift` / `repo_mismatch` / `stall` / `iter_exhausted` / `template_error`, or null if the cycle terminated cleanly |
| `failure_phase` | enum or null | non-null only when `failure_kind` is set | `pre` / `run` / `post` |

The file is written by the daemon at `cycle_started` time (with `ended_at` / terminal fields null) and replaced atomically when the cycle ends. Readers tolerate intermediate states. The on-disk shape is canonical for `roki log --meta` and the `GET /api/tickets/{id}/cycles` endpoint; the corresponding Rust type lives in the `roki-api-types` crate.

## Required elements of `requirements.md`

- **File presence**: exists
- **Non-empty**: not empty
- **Encoding sanity**: encoding is sane
- **EARS shape**: at least one EARS trigger keyword (`WHEN` / `IF` / `WHILE` / `WHERE` / `SHALL`) appears at an acceptance-criteria position

Validation is performed by the orchestrator session (`Read` + `Bash` `grep -E`) after the `materialize_spec` phase clean-exits, per [fr:19-orchestrator-session §Artifact validation](../fr/19-orchestrator-session.md). It is **structural only** (no LLM substantive judgment) — substantive judgment of "are these criteria the right ones for this ticket" lives inside `kiro-discovery`.

## Required elements of `review.md`

| Field | Type / range | Meaning |
|---|---|---|
| Overall status | `pass` or `fail` | Verdict for the whole review |
| Per-criterion entries | array | One entry for each criterion ID in the active criteria source (SPEC_DRIVEN: numeric requirement IDs in `requirements.md`; direct mode: numbered EARS sentences in the ticket body's `## Acceptance Criteria`) |
| `status` of each per-criterion entry | `pass` or `fail` | Verdict for the individual criterion |
| `code_references` of each per-criterion entry (only when status=`pass`) | One or more workspace-relative file paths (optional line range) | The code positions that justify a `pass` (must be on-disk reachable at validation time) |
| `failure_detail.category` of each per-criterion entry (only when status=`fail`) | `missing` / `regression` / `partial` / `drift` | Per-criterion failure taxonomy emitted by the producing skill (`roki-finalize-review`). Advisory: the orchestrator's structural validation does not cross-check this field, but the skill MUST emit one of the four values when the per-criterion `status` is `fail` so the artifact is parseable downstream |
| `failure_detail.diagnostic` of each per-criterion entry (only when status=`fail`) | Free text | Short human-readable failure description; not interpreted by the orchestrator |
| Frontmatter `criteria_source` (optional) | `spec_driven` / `direct` | Records the active mode at synthesis time. Advisory; not validated structurally |
| Frontmatter `target` (optional) | Feature name (SPEC_DRIVEN) or issue ID (direct mode) | Records what the artifact is reviewing. Advisory; not validated structurally |

Validation is performed by the orchestrator session (`Read` + `Bash` `test -f` for reachability) after the `finalize_review` phase clean-exits, per [fr:19-orchestrator-session §Artifact validation](../fr/19-orchestrator-session.md). The orchestrator's structural failure categories (used to populate `additional_context` on the retry path):

| Category | Condition |
|---|---|
| `fail-missing` | Artifact not present |
| `fail-schema` | Did not parse against the schema described above (missing overall status / missing per-criterion entries / criterion id not in `requirements.md`) |
| `fail-evidence` | A code reference for a `pass` entry is not reachable on disk |
| `fail-missing-spec` | `requirements.md` is missing at validation time (rare — caught by the orchestrator before nominating `finalize_review`) |

## When adding a new artifact

1. Add a row to the **Artifact list** table above.
2. Add a section listing the required elements.
3. Link to this reference from the FR pages that use it.
4. Update the corresponding requirements.

## Related reference

- [config.md](config.md): operator-facing configuration knobs
- [extension-surface.md](extension-surface.md): `OrchestratorRead`, `TrackerRefresh`, `additional_context`, reserved `WORKFLOW.md` namespaces
