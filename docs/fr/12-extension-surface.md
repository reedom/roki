---
refs:
  id: fr:12-extension-surface
  kind: fr
  title: "Extension Surface"
  spec: roki-mvp
  implements:
    - req:roki-mvp:13
  related:
    - fr:02-configuration
    - fr:03-linear-integration
    - fr:14-operator-notifications
    - fr:15-http-api
    - fr:20-rule-and-cycle-engine
    - fr:21-log-access
---

# FR 12: Extension Surface

> Downstream specs and operators integrate with roki through public, externally-observable surfaces only — the WORKFLOW.toml schema, the HTTP API, the structured event log, the per-ticket capture CLIs, and the refresh nudge endpoint. There is no in-process trait or hook system anymore. The previous `OrchestratorRead` / `TransitionSubscriber` / `TrackerRefresh` / `additional_context` injection / phase override / namespaced config surfaces are either removed or absorbed into the public surfaces.

## Purpose

Earlier versions exposed six in-process surfaces (`OrchestratorRead`, `TransitionSubscriber`, `TrackerRefresh`, the engine adapter's `additional_context` channel, per-phase command / template overrides, and `extension.<spec>.*` namespaced configuration) so downstream specs (currently observability) could integrate without forking the orchestrator core. The pivot to a config-driven engine ([20-rule-and-cycle-engine](20-rule-and-cycle-engine.md)) collapses every one of those into either a public observability surface or an operator-authored entry in WORKFLOW.toml. There is no longer a roki-specific "extension trait surface" — extension is just operator authoring plus public APIs.

## User-visible Behavior

### Mapping from old surfaces to new

| Old surface | Replacement |
|---|---|
| `OrchestratorRead` (read-only snapshot of per-issue state) | HTTP API endpoints under `/api/tickets`, `/api/tickets/{id}`, `/api/tickets/{id}/cycles` ([15-http-api](15-http-api.md)) |
| `TransitionSubscriber` (subscribe to per-issue state-machine transitions) | Structured event log + ring buffer + `GET /api/events` ([13-observability-logs](13-observability-logs.md), [15-http-api](15-http-api.md)) |
| `TrackerRefresh` (request a Linear poll) | `POST /api/refresh` (or equivalent) — the refresh nudge endpoint ([03-linear-integration §Refresh nudge](03-linear-integration.md)) |
| Engine adapter `additional_context` injection | Template variables `{{ pre.* }}` / `{{ post.* }}` / `{{ run.* }}` populated from operator-authored response payloads ([20-rule-and-cycle-engine §Inter-phase data flow](20-rule-and-cycle-engine.md)) |
| Phase override (`extension.phase.<name>.command` / `prompt_template_<phase>`) | Operator authors `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` entries directly; there is no built-in phase catalog to override |
| Namespaced configuration (`extension.<spec>.*`) | Operators put whatever they want into WORKFLOW.toml; the daemon does not reserve namespaces |

### Public surfaces in scope of this FR

- **WORKFLOW.toml schema** ([02-configuration](02-configuration.md)): the dispatch table for everything roki does. Hot-reloadable. Per-repo splits via `[[admission.repos]] workflow = "..."`.
- **HTTP API** ([15-http-api](15-http-api.md)): read-only access to the diff cache, cycle history, structured events, escalation queue, and a refresh-nudge endpoint.
- **Structured event log** ([13-observability-logs](13-observability-logs.md)): JSON Lines stream available on stdout, file, the HTTP API ring buffer, and `roki events`.
- **Per-ticket capture CLIs** ([21-log-access](21-log-access.md)): `roki log`, `roki events`, `roki repo`. Storage layout opaque; CLIs stable.
- **Operator-installed cli lines**: every phase's tool surface comes from the operator-authored cli line ([11-agent-tool-boundary](11-agent-tool-boundary.md)). The daemon does not enforce or expose per-tool granularity.

### Invariants preserved across the dissolution

- **Read-only by default**: HTTP API endpoints do not mutate state. The single mutating endpoint is the refresh nudge, which only schedules a poll subject to the cadence cap and 429 backoff.
- **No bypass of the cadence cap / 429 backoff**: the refresh nudge honors both.
- **Failure isolation**: a misbehaving event subscriber on the HTTP API does not block the daemon (HTTP delivery failures are recorded but do not stop the cycle engine).
- **Round-trip unknown keys**: WORKFLOW.toml's loader rejects keys it does not recognize at the schema level, but per-rule `outcome` strings, operator-defined response fields, and per-rule extra keys under `when.*` matchers (where the matcher itself is recognized) round-trip.
- **Secret isolation**: secrets in `roki.toml` never appear on the HTTP API, in event payloads, or in capture files; the redaction layer in [13-observability-logs](13-observability-logs.md) enforces this.

### What downstream specs can rely on

A downstream spec (e.g. roki-observability) integrates by:

1. Reading state through the HTTP API.
2. Subscribing to events through the HTTP API ring buffer or the structured event log destination.
3. Authoring `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` entries that invoke whatever tooling the spec needs.

There is no in-process trait the spec implements; the integration is process-external.

## Capabilities

- **One public observability surface**: HTTP API + event log + capture CLIs cover read access for every consumer.
- **One configuration surface**: WORKFLOW.toml + workflow/*.md cover all dispatch and phase customization.
- **No per-spec namespace reservation**: operators or downstream specs put whatever they need into the operator's WORKFLOW.toml; the daemon does not reserve `extension.<spec>.*`.
- **Refresh nudge preserved**: a Linear refresh can be requested without violating the cadence cap, identical to the prior `TrackerRefresh` semantics.
- **Public, stable contracts**: HTTP endpoint shapes, event field names, CLI flag names, and template variable names are the contract surface. On-disk storage layout is not.

## Boundaries

- **In-process trait / hook surfaces** are out of scope. Downstream specs do not link against the daemon binary.
- **A state-mutating subscriber API** is not provided.
- **Vetoable transition hooks** are not provided.
- **Daemon-registered agent-side tools** are not provided ([11-agent-tool-boundary](11-agent-tool-boundary.md)).
- **Daemon-registered read-only self-diagnosis tools** are not provided. Phase subprocesses that want to inspect daemon state use `roki log` / `roki events` / the HTTP API like any other client.
- **Cross-spec dependency resolution** is the responsibility of operators / spec authors; the daemon does not coordinate spec interactions.

## Traceability

- **Roadmap**: `roadmap.md` > Boundary Strategy > "Shared seams to watch" — the seams are now external (HTTP / events / CLIs) instead of in-process traits.
- **Requirements**:
  - `roki-mvp Req 13`: Cross-Spec Extension Surface — the requirement remains; the implementation is the public observability + configuration surfaces enumerated here.
  - `roki-mvp Req 6.5`: WORKFLOW schema extension — covered by [02-configuration](02-configuration.md).
- **Design**:
  - `Extension Points` section of `.kiro/specs/roki-mvp/design.md` (pending rewrite to reflect the public-surface-only model).
- **Related reference**: [config.md](../reference/config.md), [extension-surface.md](../reference/extension-surface.md) (both pending rewrite to track this dissolution).
- **Related FR**: [02-configuration](02-configuration.md), [03-linear-integration](03-linear-integration.md), [14-operator-notifications](14-operator-notifications.md), [15-http-api](15-http-api.md), [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md), [21-log-access](21-log-access.md).
