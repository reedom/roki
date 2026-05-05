---
refs:
  id: ref:config
  kind: reference
  title: "Configuration Schema"
  related:
    - ref:cli
    - fr:02-configuration
    - fr:12-extension-surface
---

# Reference: Configuration Schema

Schema for `roki.toml` (per workspace) and `WORKFLOW.md` (Liquid + Markdown, hot-reloaded).

Working samples in [`docs/examples/`](../examples/):

- [`roki.minimal.toml`](../examples/roki.minimal.toml) / [`WORKFLOW.minimal.md`](../examples/WORKFLOW.minimal.md) — smallest configuration that boots
- [`roki.annotated.toml`](../examples/roki.annotated.toml) / [`WORKFLOW.annotated.md`](../examples/WORKFLOW.annotated.md) — every key with comments

## `roki.toml` schema

Per workspace, specified with `--config <path>` ([cli.md](cli.md)).

| Block / Key | Required | Meaning | Behavior on invalid value | Used by | Requirements |
|---|---|---|---|---|---|
| `[linear].api_token` (source) | yes | Where to fetch the Linear API token from (env / file / etc.) | Refuses startup if it cannot be resolved | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 2.3 |
| `[linear].webhook_secret` (source) | yes | Where to fetch the Linear webhook HMAC secret from | Refuses startup if it cannot be resolved | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 2.3, Req 3.1 |
| `[linear].assignee` | yes | Assignee to admit. `me` resolves to the API token holder | Refuses startup on resolution failure or multiple resolutions | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 2.8, Req 2.9 |
| `[linear].admit_states` | no | Set of Linear workflow state names to admit (default `["Todo"]`) | Refuses startup on empty set | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 2.11 |
| `[linear].poll_cadence_seconds` | no | Workspace-level polling cadence floor in seconds (default `300`, the documented 5-minute cap). The daemon never issues more than one Linear poll per `poll_cadence_seconds`, even when nudged | Refuses startup on values below `60` | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 3.3, Req 3.4 |
| `[workflow].path` | yes | Path to `WORKFLOW.md` | Refuses startup if missing / unreadable | [02-configuration](../fr/02-configuration.md) | roki-mvp Req 2.4, Req 6.1 |
| `[server].bind` | no | Bind host of the webhook receiver (overridable via CLI `--bind`) | Refuses startup on bind failure | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 2.5 |
| `[server].port` | no | Bind port of the webhook receiver (overridable via CLI `--port`) | Refuses startup on bind failure | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 2.5 |
| `[[repos]].ghq` | 0+ | `ghq` identifier of an allowlisted repo (`owner/repo` or `host/owner/repo`) | Refuses startup on duplicates; an empty allowlist still boots (the orchestrator's `act` admission decisions then fail allowlist validation) | [06-worktree-and-session](../fr/06-worktree-and-session.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 2.1, Req 2.2, Req 2.7 |
| `[permissions].strategy` | yes | `--settings` allowlist or `--dangerously-skip-permissions` (also overridable via CLI flag); applies to phase subprocesses only — orchestrator session always runs with a read-only filesystem sandbox | Refuses startup if not set | [07-worker-execution](../fr/07-worker-execution.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 9.3, Req 9.4, Req 9.5, Req 9.6 |

`roki.toml` itself is **not hot-reloaded** (a restart is required).

## `WORKFLOW.md` schema

Per workspace, Liquid + Markdown, hot-reload supported. Composed of front matter (YAML or TOML) and template blocks.

### Front matter / structure

`WORKFLOW.md` exposes one required named template block (the orchestrator-session system prompt) plus zero or more optional per-phase template blocks. The per-phase blocks are operator overrides; without them the daemon uses each phase's catalog default invocation (a slash-command-driven skill or a daemon-internal prompt fragment) per [18-worker-skill-workflow §Phase override](../fr/18-worker-skill-workflow.md). A per-phase block is mutually exclusive with the `extension.phase.<name>.command` slash-command override for the same phase.

| Key | Required | Meaning | Used by | Requirements |
|---|---|---|---|---|
| `prompt_template_orchestrator` (named template block) | yes | System prompt for the orchestrator session. Rendered against the issue context once per orchestrator launch with the per-ticket `mode` flag (`SPEC_DRIVEN` or `NEEDS_CLASSIFY`) substituted in (per [04-state-machine-and-recovery §Pre-admission judge](../fr/04-state-machine-and-recovery.md)); the orchestrator consumes it as it processes `phase_complete`, `phase_nonclean`, `daemon_directive`, and `tracker_terminal` events, including SPEC_DRIVEN target spec doc validation on the first turn and `review.md` validation after `finalize_review` clean exit | [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 6.1, Req 6.6 |
| `prompt_template_implement_direct` (named template block) | yes | Daemon-internal prompt for the `implement` phase in NEEDS_CLASSIFY (Path B / direct) mode. Rendered against the per-phase context envelope (ticket body's numbered `## Acceptance Criteria` as the sole spec source, plus any prior reviewer findings on retry) and written to the phase subprocess's stdin | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 6.1 |
| `prompt_template_validate_direct` (named template block) | yes | Daemon-internal prompt for the `validate` phase in NEEDS_CLASSIFY (Path B / direct) mode. Drives the two-stage mechanical / acceptance check against the ticket body's EARS criteria | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md) | roki-mvp Req 6.1 |
| `prompt_template_open_pr` (named template block) | yes | Daemon-internal prompt for the `open_pr` phase. Drives `gh pr create` with the orchestrator-supplied summary | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md) | roki-mvp Req 6.1 |
| `prompt_template_<phase>` (named template block, per phase) | no | Operator override for any other phase subprocess's prompt; rendered against the per-phase context envelope and written to the subprocess's stdin (the daemon launches `claude --input-format stream-json --output-format stream-json` instead of `claude -p '<slash-command>'`). Mutually exclusive per phase with `extension.phase.<name>.command`. Absent: the daemon uses the catalog default invocation per [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md) | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [02-configuration](../fr/02-configuration.md) | roki-mvp Req 6.7 |

### Reserved extension namespaces

Each downstream spec consumes only its own namespace. The loader **round-trips unknown keys** (does not interpret them, does not delete them).

| Namespace / Key | Consuming spec | Required | Meaning | Used by | Requirements |
|---|---|---|---|---|---|
| `extension.orchestrator.model` | roki-mvp (orchestrator session) | no | Claude model identifier for the orchestrator (default `"claude-opus-4-7"`) | [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 2.11 |
| `extension.orchestrator.effort` | roki-mvp (orchestrator session) | no | Extended-thinking budget for the orchestrator; one of `low` / `middle` / `high` (default `"middle"`) | [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 2.11 |
| `extension.orchestrator.max_phases` | roki-mvp (orchestrator session) | no | Total phase subprocesses the orchestrator may nominate before the budget routes the issue to `Inactive(reason=orchestrator_budget_exhausted)` (default `15`; lowered from the prior `20` since the per-issue `materialize_spec` phase is removed and `classify` runs at most once per ticket) | [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 2.11, Req 5.5 |
| `extension.orchestrator.allowed_tools` | roki-mvp (orchestrator session) | no | Allowlist passed to the orchestrator via `--settings` (default permits Linear MCP write + `Read` + `Bash`; `Bash` runs inside a read-only filesystem sandbox and is intended for artifact validation) | [19-orchestrator-session](../fr/19-orchestrator-session.md), [11-agent-tool-boundary](../fr/11-agent-tool-boundary.md) | roki-mvp Req 2.11, Req 5.1 |
| `extension.orchestrator.stall_seconds` | roki-mvp (orchestrator session) | no | Orchestrator-stall window in seconds (default `600`). If the orchestrator emits no stdout for this many seconds the daemon SIGTERMs it and routes the issue to `Inactive(reason=orchestrator_crash)` per Req 5.3. Default is sized for `effort=high` turns that combine extended-thinking blocks with tool calls; operators on `effort=middle`/`low` may lower the value | [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 2.11, Req 5.3 |
| `extension.phase.<name>.command` | roki-mvp (per-phase override) | no | Slash-command override for a specific phase, replacing the catalog default skill while keeping `claude -p '<command>' --output-format stream-json --max-turns N` as the invocation pattern. `<name>` is one of `classify`, `implement`, `review`, `validate`, `open_pr`, `ci_fix`, `finalize_review`. Mutually exclusive per phase with the matching `prompt_template_<phase>` named template block; declaring both is a configuration error per Req 6.7 | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [02-configuration](../fr/02-configuration.md) | roki-mvp Req 6.7, Req 13.5 |
| `extension.phase.<name>.max_turns` | roki-mvp (per-phase override) | no | Per-phase `--max-turns` override; replaces the catalog default for the named phase. Additive scalar — may be set with or without `command`, coexists with `prompt_template_<phase>` | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md) | roki-mvp Req 6.7 |
| `extension.phase.<name>.stall_seconds` | roki-mvp (per-phase override) | no | Per-phase stall window in seconds (default `120` for every phase). If the named phase emits no stream-json events for this many seconds the daemon SIGTERMs the phase subprocess and emits `phase_nonclean(stall)` to the orchestrator; if the orchestrator is no longer alive the daemon routes the issue to `Inactive(reason=stall)` and surfaces via TUI escalation queue only. Additive scalar — may be set with or without `command`, coexists with `prompt_template_<phase>` | [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md), [07-worker-execution](../fr/07-worker-execution.md), [14-operator-notifications](../fr/14-operator-notifications.md) | roki-mvp Req 5.7, Req 6.7 |
| `extension.server.port` | roki-observability | no | HTTP API port (omitting disables the API) | [15-http-api](../fr/15-http-api.md) | roki-observability Req 1.1, Req 1.2, Req 15.2 |
| `extension.server.bind` | roki-observability | no | HTTP API bind host (default `127.0.0.1`) | [15-http-api](../fr/15-http-api.md) | roki-observability Req 7.1, Req 15.2 |
| `extension.server.min_refresh_interval_seconds` | roki-observability | no | Minimum coalescing interval for `POST /refresh` | [15-http-api](../fr/15-http-api.md) | roki-observability Req 4.4, Req 15.2 |
| `extension.server.max_event_log_per_issue` | roki-observability | no | Maximum length of the event log returned by the per-issue endpoint | [15-http-api](../fr/15-http-api.md) | roki-observability Req 3.6, Req 15.2 |

The following legacy keys are removed and **explicitly refused** by the loader with an actionable error naming the offending key path per `roki-mvp Req 2.12`:

| Legacy key | Source | Replaced by |
|---|---|---|
| `[judge].model` | `roki.toml` | `extension.orchestrator.model` (orchestrator session absorbs the setup-judge) |
| `extension.linear_updater.*` | `WORKFLOW.md` | Orchestrator session writes Linear directly via the operator's installed Linear MCP (linear-updater subagent removed) |
| `extension.gates.spec.*` / `extension.gates.review.*` (and any other `extension.gates.*` key) | `WORKFLOW.md` | Orchestrator session performs structural validation of `requirements.md` and `review.md` (daemon-side mechanical gates removed) |
| `extension.distill.*` | `WORKFLOW.md` | No replacement — the per-issue `materialize_spec` distill flow is removed; SPEC_DRIVEN tickets reuse a project-level spec, NEEDS_CLASSIFY tickets use the ticket body's EARS criteria |

All four are rejected at startup. At hot reload the loader retains the previous policy and logs the failure. See [19-orchestrator-session](../fr/19-orchestrator-session.md) for how the orchestrator session absorbs these functions.

### Hot reload and validation

- **Schema validation failure at startup** → refuse to start + log the offending key path
- **Validation passes on hot reload** → apply the new policy
- **Validation fails on hot reload** → **keep the previous policy** + log the failure (do not stop the daemon)
- **Per-key invalidity inside `extension.*`** (e.g. negative `extension.orchestrator.max_phases`) → the corresponding spec refuses evaluation + logs the misconfiguration

## When adding a new key / namespace

1. Add a row to the corresponding table above.
2. Link to this table from the FR page that uses it.
3. Update `roki-mvp Req 2` (for `roki.toml`) or `roki-mvp Req 13.5` (for a `WORKFLOW.md` namespace) and the consuming spec's requirements.

## Related

- [cli.md](cli.md): override via CLI flags
- [extension-surface.md](extension-surface.md): extension contract including WORKFLOW.md namespaces
- [`docs/examples/`](../examples/): working samples
