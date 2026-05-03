---
refs:
  id: fr:12-extension-surface
  kind: fr
  title: "Extension Surface"
  spec: roki-mvp
  implements:
    - req:roki-mvp:13
---

# FR 12: Extension Surface

> The traits, hooks, and contracts that downstream specs use to plug in without forking the orchestrator core.
> The full canonical list of surfaces lives in [`docs/reference/extension-surface.md`](../reference/extension-surface.md).

## Purpose

Let downstream specs (gates / observability / distill) subscribe / veto / nudge / inject context against roki-mvp's per-issue lifecycle, without standing up their own Linear writes / DB / separate orchestrator. By having every downstream spec ride on the same contract, they stack additively without interfering with each other.

## User-visible Behavior

### Five kinds of surface

| Kind | Role |
|---|---|
| **Read** (`OrchestratorRead` trait) | Read-only snapshot of per-issue state + single-issue lookup |
| **Veto** (vetoable transition hooks, pre-cleanup hook) | Allow/deny the specified transition |
| **Nudge** (`TrackerRefresh` trait) | Request a poll while respecting the cadence cap and 429 backoff |
| **Inject** (engine adapter `additional_context`) | Inject machine-extractable additional context into the worker prompt |
| **Namespaced config** (`WORKFLOW.md` reserved namespaces) | Each spec gets its own configuration keys |

The exact signatures of each surface, the invariants that must not be bypassed, and the FR pages that consume them live in the table in [`docs/reference/extension-surface.md`](../reference/extension-surface.md).

### Invariants common to all surfaces

- **Read-only by default**: neither `OrchestratorRead` nor the `kiro_*_status` tools (read-only tools registered by downstream) mutate state.
- **Veto is limited to documented transitions**: a subscriber cannot veto an arbitrary transition. Vetoable transitions are listed explicitly.
- **No bypass of the cadence cap / backoff**: nudges via `TrackerRefresh` honor the Linear rate limit.
- **Failure isolation**: a subscriber's unhandled error does not stop other subscribers or the orchestrator core.
- **Additive serialization**: adding a new key to `additional_context` is OK; removing or retyping an existing key is breaking.
- **Round-trip unknown keys**: the `WORKFLOW.md` loader holds unknown keys without interpreting them.

## Capabilities

- **Read + veto + nudge + context inject + namespaced config** is sufficient for the five kinds of needs.
- **No new persistent surface**: downstream specs also do not require persistent storage.
- **Failure isolation**: an exception in a subscriber does not stop the core.
- **Stable contract**: the surface itself is owned by roki-mvp; downstream consumes only.

## Boundaries

- **A state-mutating subscriber API** is not provided (read + veto only).
- **Daemon-registered mutating agent-side tools** are not provided ([11-agent-tool-boundary](11-agent-tool-boundary.md)).
- **Cross-spec dependency resolution** is the responsibility of spec authors (this surface is only the technical contract).
- **Per-spec extension of the surface itself** is out of scope (adding a new surface is debated on the roki-mvp side).

## Traceability

- **Roadmap**: `roadmap.md` > Boundary Strategy > "Shared seams to watch"
- **Requirements**:
  - `roki-mvp Req 13`: Cross-Spec Extension Surface
  - `roki-mvp Req 8.3`: existence of vetoable hooks
  - `roki-mvp Req 6.5`: WORKFLOW.md schema extension
- **Design**:
  - `Extension Points` section of `.kiro/specs/roki-mvp/design.md`
- **Related reference**: [extension-surface.md](../reference/extension-surface.md), [config.md](../reference/config.md)
- **Related FR**: 02-configuration, 04-state-machine-and-recovery, 03-linear-integration, 08-pre-implementation-gate, 09-pre-pr-gate, 10-post-merge-distill, 11-agent-tool-boundary, 15-http-api
