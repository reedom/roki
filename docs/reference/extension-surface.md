# Reference: Extension Surface

The **canonical reference** for the traits / hooks / context channels that downstream specs use to plug in without forking the orchestrator core.

## Surface list

| Surface | Kind | Purpose | Used by | Requirements |
|---|---|---|---|---|
| `OrchestratorRead` trait | Read-only trait | Per-issue state snapshot + single-issue lookup | [15-http-api](../fr/15-http-api.md) | roki-mvp Req 13.1 |
| Vetoable transition hook (`Queued -> Active`) | Vetoable hook | Pre-implementation gate | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-mvp Req 8.3, roki-spec-gate Req 1 |
| Vetoable transition hook (`AwaitingReview -> TerminalSuccess`) | Vetoable hook | Pre-PR gate | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-mvp Req 8.3, roki-review-gate Req 1 |
| Pre-cleanup vetoable hook (terminal success → workspace cleanup) | Vetoable hook | Post-merge distill | [10-post-merge-distill](../fr/10-post-merge-distill.md) | roki-mvp Req 13.2, roki-distill-postmerge Req 1 |
| `TrackerRefresh` trait | Nudge trait | Out-of-cycle poll request | [15-http-api](../fr/15-http-api.md), [16-roki-tui](../fr/16-roki-tui.md) | roki-mvp Req 13.3 |
| Engine adapter `additional_context` field | Additive context channel | Inject machine-extractable additional context into the worker prompt | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) (failing findings) | roki-mvp Req 13.4, roki-review-gate Req 5.2 |
| `WORKFLOW.md` reserved namespaces | Config namespace | Each spec gets its own configuration keys | All namespaces are listed in [config.md](config.md) | roki-mvp Req 6.5, Req 13.5 |

## Contract for each surface

### `OrchestratorRead` trait

- Strictly **read-only**. Does not grant state-mutation rights.
- Exposes a snapshot + single-issue lookup.
- To prevent duplication of internal types, types exposed via the API are mapped through a projection layer.

### Vetoable transition hooks

- Declared-vetoable transitions are documented (see the table above).
- Subscribers return `Allow` / `Deny`. A `Deny` blocks the transition and is recorded in the structured log.
- Concurrent evaluations of the same `(repo, issue)` are **serialized** by the orchestrator side.
- Even when a subscriber raises an unhandled error, **failure isolation** prevents other subscribers and the orchestrator from stopping.

### Pre-cleanup vetoable hook

- Provides a window for deferred work after terminal success, while the worktree + session tempdir still exist.
- While the distill phase is returning `Deny`, cleanup is **held**.

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

### `WORKFLOW.md` reserved namespaces

For the detailed keys of each namespace, see the "Reserved extension namespaces" table in [config.md](config.md).
The reserved namespaces are:

- `extension.gates.spec.*` (roki-spec-gate)
- `extension.gates.review.*` (roki-review-gate)
- `extension.server.*` (roki-observability)
- `extension.distill.*` (roki-distill-postmerge)

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
- [artifacts.md](artifacts.md): the terminal cleanup gated by the pre-cleanup hook
- [log-events.md](log-events.md): structured log events for vetoable decisions
