---
refs:
  id: fr:10-post-merge-distill
  kind: fr
  title: "Post-Merge Distill"
  spec: roki-distill-postmerge
  implements:
    - req:roki-distill-postmerge:1
    - req:roki-distill-postmerge:2
    - req:roki-distill-postmerge:3
    - req:roki-distill-postmerge:4
    - req:roki-distill-postmerge:5
    - req:roki-distill-postmerge:6
    - req:roki-distill-postmerge:7
    - req:roki-distill-postmerge:8
    - req:roki-distill-postmerge:9
    - req:roki-distill-postmerge:10
    - req:roki-distill-postmerge:11
    - req:roki-distill-postmerge:12
    - req:roki-distill-postmerge:13
---

# FR 10: Post-Merge Distill

> A post-terminal phase wedged between `TerminalSuccess` and workspace deletion. The agent classifies flow-type artifacts (kiro `design.md` / `tasks.md`, superpowers spec, plan output, scratch notes) into **delete / archive / distill**, processes them, and the daemon validates the manifest before allowing cleanup.

## Purpose

Every ticket leaves flow-type artifacts behind. Letting them pile up creates noise; deleting them blindly loses long-term context. This feature runs the sweep after a merge as "the last chance before the workspace is destroyed", and routes artifacts into structured dispositions. The daemon plays the role of "last gatekeeper of the containment boundary" via the manifest schema + path safety, while leaving the judgment (what to keep, delete, or extract) to the agent.

## User-visible Behavior

### Wedging the phase in

- **Trigger**: when the per-`(repo, issue)` state machine transitions to `TerminalSuccess`, enqueue a distill sweep turn before the workspace deletion step (using the pre-cleanup vetoable hook → [04-state-machine-and-recovery](04-state-machine-and-recovery.md)).
- **During the sweep**: hold the workspace deletion step.
- **Cancellation**: if `TerminalSuccess` is reversed back to `Active` by another subscriber's veto or a tracker event, cancel the sweep before enqueue.
- **The daemon does not poll GitHub**: neither the `gh` CLI nor the GitHub API is called from the daemon for the purpose of triggering distill. The agent observes Linear `Done` and conveys the terminal state to the daemon through the existing roki-mvp tracker path (the agent may use `gh pr view --json mergedAt` within its own permission scope; the daemon does not intermediate this).

### Sweep turn

- **Dispatch**: the daemon dispatches one turn against the existing per-issue Claude Code session inside the issue's workspace.
- **Sweep turn budget**: configurable independently of the implementation phase budget.
- **Stall detection**: applies the same stall window as roki-mvp.
- **Failure**: if the turn exits non-cleanly / exhausts its budget without a valid manifest, the distill phase fails + workspace is preserved + cleanup is refused + waits for operator intervention + a Slack notification is fired.

### Artifact discovery and classification

- **Discovery paths**: `extension.distill.paths` in `WORKFLOW.md` (a list of workspace-relative patterns). Repos that do not configure this use the documented default (which at minimum includes `.kiro/specs/<issue>/`).
- **Classification rules**: `extension.distill.routes` (a map of path / filename pattern → disposition).
- **Hot reload**: discovery paths and routes are hot-reloaded; new values apply on the next sweep. On a schema failure, the previous policy is retained + logged.
- **Three dispositions**:
  - **`delete`** — delete an ephemeral run-only artifact from the discovered path; record a deletion marker in the manifest.
  - **`archive`** — **move verbatim** into `.kiro/archive/<issue>/` (the original relative path is preserved / mirrored under the archive root).
  - **`distill`** — extract the canonical content into a **stable home** (the destination declared in `distill.routes`, e.g. a project-level EARS / decisions directory).
- **Default-conservative**: when a rule is ambiguous / there is no matching rule, choose `archive` and record the "nature of the ambiguity" in the reason (no silent information loss).
- **Optional inputs**: the agent may consult `requirements.md` ([08-pre-implementation-gate](08-pre-implementation-gate.md)) and `review.md` ([09-pre-pr-gate](09-pre-pr-gate.md)) as classification inputs if they exist.
- **Failure to execute a disposition**: do not silently drop; record "original artifact path + attempted disposition + structured failure reason" in the manifest.
- **Write boundary**: do not write outside the workspace root or the configured project archive root.

### Manifest

The exact path of `distill-manifest.json`, the top-level fields, the entry fields, and the archive root rule are described in [`docs/reference/artifacts.md`](../reference/artifacts.md).
Highlights:

- **Path**: `.kiro/specs/<issue>/distill-manifest.json`, written **exactly once** per sweep turn as a **complete single write** (no partial / streaming writes).
- **Structure**: three top-level fields: `schema_version` / `entries` / `summary`. Each entry contains the original path / disposition / destination or deletion marker / rule source / timestamp.
- **Archive root**: `.kiro/archive/<issue>/` is the root, with the original workspace-relative path mirrored under it.

### Daemon-side validation

- **Schema validation**: validate against the published JSON-Schema for the relevant `schema_version`.
- **Path-safety check**: every path in the manifest is checked against the path-safety invariant from [06-worktree-and-session](06-worktree-and-session.md). Escapes via symlink / hardlink are rejected after canonicalization.
- **On failure** (schema fail / path-safety fail): the distill phase fails + workspace is preserved + the offending field/path is logged + cleanup is refused + waits for operator intervention.
- **No content interpretation**: the artifact contents are not read; no LLM is called; the daemon does not classify on its own.

### Idempotency and recovery

- **Re-sweep**: at the start of the sweep, the agent checks for the existence of `distill-manifest.json` → if a recognizable `schema_version` is present, treat the sweep as already complete; do not write a new manifest; do not touch any artifact.
- **Re-activation**: if the daemon re-activates the phase, a valid manifest is accepted as a validation input and cleanup proceeds (the sweep is not re-dispatched).
- **Unknown `schema_version`**: the distill phase fails + workspace is preserved + the unknown version is logged. **The manifest is not overwritten.**
- **Source of truth**: the manifest is the record of truth. On a re-run, the agent does not assume that disposition results "still exist on disk", and does not retry execution.
- **Failure recovery**: in the failed state, retain the workspace, do not auto-retry, and wait for operator action. After the operator manually clears the failed manifest / configures a re-run, the next activation allows a fresh sweep turn. For unchanged completed manifests, idempotency is honored.

## Capabilities

- **Vetoable seam**: a pre-cleanup hook wedged between `TerminalSuccess` and workspace deletion ([12-extension-surface](12-extension-surface.md)).
- **Versioned schema**: schema evolution is supported.
- **Predictable archive layout**: `find .kiro/archive/<issue>/` deterministically reveals the archive structure.
- **Single tracing pipeline**: structured logs from the distill phase flow through the same pipeline as roki-mvp ([13-observability-logs](13-observability-logs.md)).
- **No content leakage in logs**: artifact contents are not logged; only the artifact path and the manifest's structured fields are.

## Boundaries

- **Linear / GitHub write operations** are not performed in this phase.
- **Daemon-side LLM judgment** is absent (only structural checks).
- **Auto-commit / auto-PR** are out of scope (when distill produces commit-worthy changes, the agent is expected to open a follow-up PR in a separate turn).
- **Pre-implementation distill** (e.g. EARS merge) belongs to [08-pre-implementation-gate](08-pre-implementation-gate.md).
- **Real-time distill** (sweep during implementation) is out of scope.
- **Cross-issue / project-level distillation** is out of scope (a future spec).
- **Multiple manifests** (per-attempt history, etc.) are out of scope (one per sweep).

## Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-distill-postmerge`; Boundary Strategy > "Distill (post-merge flow-doc sweep)"
- **Requirements**:
  - `roki-distill-postmerge Req 1` - `Req 13`: phase activation / merge signal / sweep dispatch / discovery / classification / execution / manifest / validation / idempotency / archive scheme / path safety / failure recovery / observability
  - `roki-mvp Req 13.2`: pre-cleanup vetoable hook
- **Design**:
  - `.kiro/specs/roki-distill-postmerge/design.md`
- **Related reference**: [artifacts.md](../reference/artifacts.md) (`distill-manifest.json` schema, archive root), [config.md](../reference/config.md) (`extension.distill.*`), [extension-surface.md](../reference/extension-surface.md) (pre-cleanup hook), [log-events.md](../reference/log-events.md) (distill phase events)
- **Related FR**: 04-state-machine-and-recovery, 06-worktree-and-session, 12-extension-surface, 08-pre-implementation-gate, 09-pre-pr-gate, 13-observability-logs
