---
refs:
  id: ref:extension-surface
  kind: reference
  title: "Extension Surface"
  related:
    - fr:12-extension-surface
    - ref:config
---

# Reference: Extension Surface

The **canonical reference** for the traits / hooks / context channels that downstream specs use to plug in without forking the orchestrator core.

## Surface list

| Surface | Kind | Purpose | Used by | Requirements |
|---|---|---|---|---|
| `OrchestratorRead` trait | Read-only trait | Per-issue state snapshot + single-issue lookup + escalation queue snapshot | [15-http-api](../fr/15-http-api.md) | roki-mvp Req 13.1 |
| Vetoable transition hook (`Judging -> Active`) | Vetoable hook | Pre-implementation gate | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-mvp Req 8.3, roki-spec-gate Req 1 |
| Vetoable transition hook (`Active -> Inactive`) | Vetoable hook (with `Deny+RetryWithContext`) | Pre-PR gate, including fix-finding re-launch | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-mvp Req 8.3, roki-review-gate Req 1 |
| `TrackerRefresh` trait | Nudge trait | Out-of-cycle poll request | [15-http-api](../fr/15-http-api.md), [16-roki-tui](../fr/16-roki-tui.md) | roki-mvp Req 13.3 |
| Engine adapter `additional_context` field | Additive context channel | Inject machine-extractable additional context into the worker prompt | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) (failing findings on `Deny+RetryWithContext`) | roki-mvp Req 13.4, roki-review-gate Req 5.2 |
| Engine adapter linear-updater dispatch | Internal mvp surface | Translate daemon-only failure events into Linear label/comment writes via the operator's MCP | [14-operator-notifications](../fr/14-operator-notifications.md) | roki-mvp Req 5.10, Req 12 |
| `WORKFLOW.md` reserved namespaces | Config namespace | Each spec gets its own configuration keys | All namespaces are listed in [config.md](config.md) | roki-mvp Req 6.5, Req 13.5 |

## Contract for each surface

### `OrchestratorRead` trait

- Strictly **read-only**. Does not grant state-mutation rights.
- Exposes a per-issue snapshot, a single-issue lookup, and a snapshot of the escalation queue (the daemon-only failure surface populated alongside linear-updater dispatch).
- To prevent duplication of internal types, types exposed via the API are mapped through a projection layer.

### Vetoable transition hooks

- Two declared-vetoable transitions: `Judging → Active` (spec gate) and `Active → Inactive` (review gate).
- Subscribers return `Allow` or `Deny`. The review gate may additionally return `Deny+RetryWithContext(payload)`, which the orchestrator turns into a re-launch of the worker subprocess with `payload` forwarded via `additional_context` (this is how the fix-finding loop is implemented without a dedicated state).
- A plain `Deny` blocks the transition and is recorded in the structured log; the orchestrator's downstream behavior is per the originating spec (e.g. spec-gate `Deny` exhausted → `Inactive(reason=judge_unparseable)`; review-gate `Deny` exhausted → `Inactive(reason=review_gate_exhausted)`).
- Concurrent evaluations of the same issue are **serialized** by the orchestrator side.
- Even when a subscriber raises an unhandled error, **failure isolation** prevents other subscribers and the orchestrator from stopping.

### `TrackerRefresh` trait

- Lets a caller request an out-of-cycle poll.
- **Does not bypass the cadence cap (5 min) or the 429 backoff state**.
- Requests during backoff are queued and fire at the end of backoff. The caller may receive an estimate.
- Synchronous bursts within the documented minimum interval are **coalesced**.

### Engine adapter `additional_context`

- An optional additive field on `WorkerContext`.
- Forwarded verbatim into a **stable, machine-extractable region** of the worker subprocess's prompt input.
- Lives in a region separate from the rendered output of `prompt_template_worker`; the daemon does not interpret its contents.
- The serialization format is defined by the engine adapter design and is **additive** (adding a new key is OK; deleting or retyping an existing key is breaking).
- Primary consumer today: the review gate's fix-finding `payload` on `Deny+RetryWithContext`.

### Engine adapter linear-updater dispatch

- Internal to roki-mvp; not consumed by downstream specs directly. Listed here so its place in the seam taxonomy is visible.
- Translates a structured directive (`{ issue_id, kind, fields, timestamp }`) into a one-shot bounded `claude` invocation that renders `prompt_template_linear_updater` and uses the operator's installed Linear MCP to add labels and post comments. The daemon never writes Linear directly.
- Triggered automatically on transitions into `failure`-flavored `Inactive(reason=...)` and on judge-side rejections (`needs_split`, `allowlist_rejected`); see [14-operator-notifications](../fr/14-operator-notifications.md) for the canonical trigger list.

### `WORKFLOW.md` reserved namespaces

For the detailed keys of each namespace, see the "Reserved extension namespaces" table in [config.md](config.md).
The reserved namespaces are:

- `extension.gates.spec.*` (roki-spec-gate)
- `extension.gates.review.*` (roki-review-gate)
- `extension.server.*` (roki-observability)
- `extension.linear_updater.*` (roki-mvp linear-updater subagent)

The loader merely round-trips unknown keys; it does not interpret them.

## When adding a new surface

Adding a new surface requires **agreement on the roki-mvp side** (downstream cannot extend on its own).
Steps for additions:

1. Add a row to the **Surface list** table above.
2. Add a section under "Contract for each surface" describing the new surface (semantics, invariants that must not be bypassed, failure-isolation rules).
3. Link to this reference from the FR pages that use it.
4. Update `roki-mvp Req 13` and the consuming spec's requirements.

## Related reference

- [config.md](config.md): details of the WORKFLOW.md reserved namespaces
- [log-events.md](log-events.md): structured log events for vetoable decisions and linear-updater outcomes
