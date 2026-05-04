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

Traits / hooks / context channels that downstream specs use to plug in without forking the orchestrator core.

## Surface list

| Surface | Kind | Purpose | Used by | Requirements |
|---|---|---|---|---|
| `OrchestratorRead` trait | Read-only trait | Per-issue state snapshot (including the three orchestrator-dead `Inactive.reason` values) + single-issue lookup + escalation queue snapshot | [15-http-api](../fr/15-http-api.md) | roki-mvp Req 13.1 |
| `TrackerRefresh` trait | Nudge trait | Out-of-cycle poll request | [15-http-api](../fr/15-http-api.md), [16-roki-tui](../fr/16-roki-tui.md) | roki-mvp Req 13.3 |
| Engine adapter `additional_context` field | Additive context channel | Inject machine-extractable additional context into a phase subprocess's prompt envelope (kept distinct from the skill's installed prompt body). The orchestrator populates this on `action=run_phase` directives, including the artifact-validation retry path (e.g. failing per-criterion entries the orchestrator read from `review.md` injected into the next `implement` phase) | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 13.4 |
| Phase override (`extension.phase.<name>.command` / `prompt_template_<phase>`) | Per-phase invocation surface | Operator override of any phase's catalog default. Two mutually exclusive forms per phase: `extension.phase.<name>.command` swaps the slash-command-driven skill while keeping the daemon's invocation pattern; `prompt_template_<phase>` (named template block) replaces the prompt entirely and is rendered onto the subprocess's stdin. Default invocation is restored when neither form is declared. Mutually exclusive: declaring both for one phase is a configuration error rejected at startup or retained as previous policy at hot reload | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [02-configuration](../fr/02-configuration.md) | roki-mvp Req 6.7, Req 13.5 |
| Engine adapter orchestrator session dispatch | Internal mvp surface | Long-lived `claude --input-format stream-json --output-format stream-json` per ticket; the daemon writes `daemon_directive` (and other) events on the orchestrator's stdin and reads strict JSON action directives on its stdout. The orchestrator absorbs admission classification, phase planning, artifact validation, and Linear writes (the prior linear-updater subagent dispatch and daemon-side mechanical kiro-spec / kiro-review gates are removed; the daemon never writes Linear directly and never decides whether an artifact passes) | [14-operator-notifications](../fr/14-operator-notifications.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 5.1, Req 5.2, Req 12 |
| `WORKFLOW.md` reserved namespaces | Config namespace | Each spec (and the orchestrator session) gets its own configuration keys | All namespaces are listed in [config.md](config.md) | roki-mvp Req 13.5 |

## Contract for each surface

### `OrchestratorRead` trait

- Strictly **read-only**. Does not grant state-mutation rights.
- Exposes a per-issue snapshot (including the `Inactive.reason` discriminator with the three orchestrator-dead values `orchestrator_crash` / `orchestrator_unparseable` / `orchestrator_budget_exhausted` per [19-orchestrator-session](../fr/19-orchestrator-session.md)), a single-issue lookup, and a snapshot of the escalation queue (the daemon-only failure surface populated alongside `daemon_directive` events sent to the orchestrator).
- To prevent duplication of internal types, types exposed via the API are mapped through a projection layer.

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
- Primary consumer today: the orchestrator's `review.md` validation retry path. After the orchestrator reads `review.md` (post `finalize_review` clean exit) and finds a structural problem or overall `status=fail`, it populates `additional_context` on its next `action=run_phase` directive (typically re-nominating `implement`) with the failure detail (failing per-criterion entries) so the next phase subprocess sees the diagnostic verbatim ([19-orchestrator-session §Artifact validation](../fr/19-orchestrator-session.md)). In SPEC_DRIVEN mode the channel also carries the resolved target spec name + project-level spec dir path on the first `implement` nomination; in NEEDS_CLASSIFY (Path B / direct) mode it carries the ticket body's `## Acceptance Criteria` block.

### Engine adapter orchestrator session dispatch

- Internal to roki-mvp; not consumed by downstream specs directly. Listed here so its place in the seam taxonomy is visible.
- Supervises the long-lived per-ticket `claude --input-format stream-json --output-format stream-json` orchestrator session: launch with `prompt_template_orchestrator` rendered as system prompt (with the per-ticket `mode` flag substituted in) and `--settings` enforcing `extension.orchestrator.allowed_tools`, write JSON events on its stdin (`phase_complete`, `phase_nonclean`, `daemon_directive`, `tracker_terminal`), parse the last JSON object on its stdout per turn against its strict action enum (`run_phase` / `linear_update_done` / `stop`).
- The orchestrator absorbs the prior architecture's setup-judge subprocess (admission classification, replaced in NEEDS_CLASSIFY mode by the `classify` phase subprocess and in SPEC_DRIVEN mode by the daemon's mechanical pre-admission-judge per [04-state-machine-and-recovery](../fr/04-state-machine-and-recovery.md)), linear-updater subagent (daemon-only failure surfacing), and daemon-side mechanical kiro-spec / kiro-review gates (artifact structural validation). The daemon translates daemon-only failure events into `daemon_directive` events on the orchestrator's stdin; the orchestrator writes the corresponding Linear label + comment via the operator's installed Linear MCP and returns `action=linear_update_done`. `review.md` validation retry-budget exhaustion and operator-facing pre-phase stops (`spec_incomplete`, `needs_operator`, `needs_split`, `allowlist_rejected`) are owned entirely by the orchestrator (no `daemon_directive` is sent for them). The daemon never writes Linear directly and never decides whether an artifact passes.
- When the orchestrator is dead (`orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`) the daemon does **not** fall back to a Linear write; the issue surfaces via the structured log + TUI escalation queue only ([14-operator-notifications](../fr/14-operator-notifications.md)).

### Phase override

- Per-phase override of the catalog default invocation defined in [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md). Two mutually exclusive forms per phase:
  - **Slash-command swap**: `extension.phase.<name>.command = "/<skill> <args...>"` replaces the catalog default skill while keeping the daemon's `claude -p '<command>' --output-format stream-json --max-turns N` invocation pattern.
  - **Template stdin replacement**: a `prompt_template_<phase>` named template block (Liquid + Markdown) replaces the prompt entirely; the daemon renders against the same per-phase variables and writes the result to the subprocess's stdin.
- The two forms are mutually exclusive per phase: declaring both for the same phase is a configuration error and is rejected at startup or retained as the previous policy at hot reload (per `roki-mvp Req 6.7`).
- Override scope is per-phase: declaring `extension.phase.implement.command` does not affect any other phase. Phases for which neither form is declared use the catalog default (slash-command skill or daemon-internal prompt fragment).
- Override applies per ticket admission of the affected phase: an in-flight phase subprocess always finishes with the configuration that was in effect when the daemon spawned it; subsequent nominations of the same phase pick up the new policy.
- Override does **not** change the daemon-side supervision contract (lifecycle observation, exit-envelope translation, sandbox profile, `--max-turns` budget). An override that needs additional tools must declare them on the operator's Claude Code allowlist independently.

### `WORKFLOW.md` reserved namespaces

For the detailed keys of each namespace, see the "Reserved extension namespaces" table in [config.md](config.md).
The reserved namespaces are:

- `extension.orchestrator.*` (roki-mvp orchestrator session — `model`, `effort`, `max_phases`, `allowed_tools`)
- `extension.phase.<name>.*` (roki-mvp per-phase override — `command`; per-phase, mutually exclusive with `prompt_template_<phase>` named template blocks)
- `extension.server.*` (roki-observability)

Per-phase named template blocks (`prompt_template_<phase>`) live alongside the required `prompt_template_orchestrator` block; they are part of the Phase override surface above.

The legacy `extension.linear_updater.*`, `extension.gates.spec.*`, and `extension.gates.review.*` namespaces are removed alongside the subprocess shapes / gates they served; their functions are absorbed by the orchestrator session. The loader merely round-trips unknown keys under the reserved namespaces; it does not interpret them.

## Removed seams

The following extension surfaces from prior architectures have been removed. They are **not** part of the current contract and must not be relied on:

| Removed seam | Removed because | Replaced by |
|---|---|---|
| `Registry::register` (per-worker spawn registry trait) | Workers no longer self-register; the orchestrator session nominates phase subprocesses via `action=run_phase` directives | Engine adapter orchestrator session dispatch (above) + the phase catalog in [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md) |
| `prompt_template_setup` (named template block) | The setup-judge subprocess is removed; admission classification runs inside the orchestrator session (SPEC_DRIVEN) or a dedicated `classify` phase subprocess (NEEDS_CLASSIFY) | `prompt_template_orchestrator` named template block (required) |
| `prompt_template_worker` (named template block) | A single per-worker prompt no longer fits the per-phase catalog; each phase has its own catalog default and override surface | Per-phase named template blocks (`prompt_template_<phase>`) and `extension.phase.<name>.command` (mutually exclusive per phase) |
| Pre-cleanup hook (vetoable cleanup callback) | Cleanup is unconditional; the orchestrator never receives a pre-cleanup callback because it cannot veto worktree / session-tempdir removal | Unconditional cleanup on `Inactive` per [06-worktree-and-session](../fr/06-worktree-and-session.md) |
| `Skipped` per-issue terminal state | Pre-admission rejections are silent (log only, no state entry) per [04-state-machine-and-recovery §Pre-admission judge](../fr/04-state-machine-and-recovery.md) | Pre-admission skipped log event (see [log-events.md](log-events.md)); no state machine entry |
| `Judging` per-issue state | The setup-judge subprocess is removed; orchestrator launch and classification are part of `Pending` | Orchestrator session lifecycle inside `Pending` per [19-orchestrator-session](../fr/19-orchestrator-session.md) |
| Daemon-side `roki-spec-gate` / `roki-review-gate` vetoable hooks | Daemon never decides whether an artifact passes; structural validation lives in the orchestrator session | Orchestrator session §Artifact validation per [19-orchestrator-session](../fr/19-orchestrator-session.md) |

## When adding a new surface

Adding a new surface requires **agreement on the roki-mvp side** — downstream cannot extend on its own.

1. Add a row to the **Surface list** table above.
2. Add a section under "Contract for each surface" describing semantics, invariants that must not be bypassed, and failure-isolation rules.
3. Link to this reference from the FR pages that use it.
4. Update `roki-mvp Req 13` and the consuming spec's requirements.

## Related reference

- [config.md](config.md): details of the WORKFLOW.md reserved namespaces
- [log-events.md](log-events.md): orchestrator lifecycle and `daemon_directive` log events
