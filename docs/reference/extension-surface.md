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
| `OrchestratorRead` trait | Read-only trait | Per-issue state snapshot (including the three orchestrator-dead `Inactive.reason` values) + single-issue lookup + escalation queue snapshot | [15-http-api](../fr/15-http-api.md) | roki-mvp Req 13.1 |
| Vetoable transition hook (`Judging -> Active`) | Vetoable hook | Pre-implementation gate | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-mvp Req 8.3, Req 13.2, roki-spec-gate Req 1 |
| Vetoable transition hook (`Active -> Inactive`) | Vetoable hook (with `Deny+RetryWithContext`) | Pre-PR gate, including fix-finding re-launch (the orchestrator translates the payload into a `gate_deny` event for A, which then nominates an `implement` phase with `additional_context` populated from the payload) | [09-pre-pr-gate](../fr/09-pre-pr-gate.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 8.3, Req 13.2, roki-review-gate Req 1 |
| `TrackerRefresh` trait | Nudge trait | Out-of-cycle poll request | [15-http-api](../fr/15-http-api.md), [16-roki-tui](../fr/16-roki-tui.md) | roki-mvp Req 13.3 |
| Engine adapter `additional_context` field | Additive context channel | Inject machine-extractable additional context into a phase subprocess's prompt envelope (kept distinct from the skill's installed prompt body); review-gate `Deny+RetryWithContext` payloads enter through this channel after A translates `gate_deny` into `run_phase` | [09-pre-pr-gate](../fr/09-pre-pr-gate.md), [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 13.4, roki-review-gate Req 5.2 |
| Engine adapter orchestrator session dispatch | Internal mvp surface | Long-lived `claude --input-format stream-json --output-format stream-json` per ticket; the daemon writes `daemon_directive` (and other) events on A's stdin and reads strict JSON action directives on A's stdout. A absorbs admission classification, phase planning, and Linear writes (the prior linear-updater subagent dispatch is removed; the daemon never writes Linear directly) | [14-operator-notifications](../fr/14-operator-notifications.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 5.1, Req 5.2, Req 12 |
| `WORKFLOW.md` reserved namespaces | Config namespace | Each spec (and orchestrator session A) gets its own configuration keys | All namespaces are listed in [config.md](config.md) | roki-mvp Req 13.5 |

## Contract for each surface

### `OrchestratorRead` trait

- Strictly **read-only**. Does not grant state-mutation rights.
- Exposes a per-issue snapshot (including the `Inactive.reason` discriminator with the three orchestrator-dead values `orchestrator_crash` / `orchestrator_unparseable` / `orchestrator_budget_exhausted` per [19-orchestrator-session](../fr/19-orchestrator-session.md)), a single-issue lookup, and a snapshot of the escalation queue (the daemon-only failure surface populated alongside `daemon_directive` events sent to A).
- To prevent duplication of internal types, types exposed via the API are mapped through a projection layer.

### Vetoable transition hooks

- Two declared-vetoable transitions: `Judging → Active` (spec gate) and `Active → Inactive` (review gate).
- Subscribers return `Allow` or `Deny`. The review gate may additionally return `Deny+RetryWithContext(payload)`, which the orchestrator turns into a `gate_deny` event on A's stdin; A then nominates an `implement` phase with `additional_context` populated from the payload (this is how the fix-finding loop is implemented without a dedicated state — see [19-orchestrator-session > Event catalog](../fr/19-orchestrator-session.md)).
- A plain `Deny` blocks the transition and is recorded in the structured log; the orchestrator's downstream behavior is per the originating spec (e.g. review-gate `Deny` exhausted → `Inactive(reason=review_gate_exhausted)` and a `daemon_directive` of the same kind sent to A so A surfaces the failure to Linear before its terminal exit).
- Concurrent evaluations of the same issue are **serialized** by the orchestrator side.
- Even when a subscriber raises an unhandled error, **failure isolation** prevents other subscribers and the orchestrator from stopping.

### `TrackerRefresh` trait

- Lets a caller request an out-of-cycle poll.
- **Does not bypass the cadence cap (5 min) or the 429 backoff state**.
- Requests during backoff are queued and fire at the end of backoff. The caller may receive an estimate.
- Synchronous bursts within the documented minimum interval are **coalesced**.

### Engine adapter `additional_context`

- An optional additive field on the per-phase context envelope.
- Forwarded verbatim into a **stable, machine-extractable region** of the phase subprocess's prompt input.
- Lives in a region separate from the skill's installed prompt body (per [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md)); the daemon does not interpret its contents.
- The serialization format is defined by the engine adapter design and is **additive** (adding a new key is OK; deleting or retyping an existing key is breaking).
- Primary consumer today: the review gate's fix-finding `payload` on `Deny+RetryWithContext`, translated into A's `additional_context` field on the next `run_phase` directive via the `gate_deny` event ([19-orchestrator-session](../fr/19-orchestrator-session.md)).

### Engine adapter orchestrator session dispatch

- Internal to roki-mvp; not consumed by downstream specs directly. Listed here so its place in the seam taxonomy is visible.
- Supervises the long-lived per-ticket `claude --input-format stream-json --output-format stream-json` orchestrator session A: launch with `prompt_template_orchestrator` rendered as system prompt and `--settings` enforcing `extension.orchestrator.allowed_tools`, write JSON events on A's stdin (`admission_request`, `phase_complete`, `phase_nonclean`, `daemon_directive`, `gate_deny`, `tracker_terminal`), parse the last JSON object on A's stdout per turn against A's strict action enum (`admission_decision` / `run_phase` / `linear_update_done` / `stop`).
- A absorbs the prior architecture's setup-judge subprocess (admission classification) and linear-updater subagent (daemon-only failure surfacing). The daemon translates daemon-only failure events into `daemon_directive` events on A's stdin; A writes the corresponding Linear label + comment via the operator's installed Linear MCP and returns `action=linear_update_done`. The daemon never writes Linear directly.
- When A is dead (`orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`) the daemon does **not** fall back to a Linear write; the issue surfaces via the structured log + TUI escalation queue only ([14-operator-notifications](../fr/14-operator-notifications.md)).

### `WORKFLOW.md` reserved namespaces

For the detailed keys of each namespace, see the "Reserved extension namespaces" table in [config.md](config.md).
The reserved namespaces are:

- `extension.orchestrator.*` (roki-mvp orchestrator session A — `model`, `effort`, `max_phases`, `allowed_tools`)
- `extension.gates.spec.*` (roki-spec-gate)
- `extension.gates.review.*` (roki-review-gate)
- `extension.server.*` (roki-observability)

The legacy `extension.linear_updater.*` namespace is removed; its function is absorbed by orchestrator session A. The loader merely round-trips unknown keys under the reserved namespaces; it does not interpret them.

## When adding a new surface

Adding a new surface requires **agreement on the roki-mvp side** (downstream cannot extend on its own).
Steps for additions:

1. Add a row to the **Surface list** table above.
2. Add a section under "Contract for each surface" describing the new surface (semantics, invariants that must not be bypassed, failure-isolation rules).
3. Link to this reference from the FR pages that use it.
4. Update `roki-mvp Req 13` and the consuming spec's requirements.

## Related reference

- [config.md](config.md): details of the WORKFLOW.md reserved namespaces
- [log-events.md](log-events.md): structured log events for vetoable decisions, A's lifecycle, and `daemon_directive` outcomes
