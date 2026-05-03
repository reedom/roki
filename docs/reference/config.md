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

The **canonical schema reference** for `roki.toml` (per workspace) and `WORKFLOW.md` (Liquid + Markdown, hot-reloaded).

For working samples, see [`docs/examples/`](../examples/):

- [`roki.minimal.toml`](../examples/roki.minimal.toml) / [`WORKFLOW.minimal.md`](../examples/WORKFLOW.minimal.md) — the smallest configuration that boots (usable as a starting point with `cp`)
- [`roki.annotated.toml`](../examples/roki.annotated.toml) / [`WORKFLOW.annotated.md`](../examples/WORKFLOW.annotated.md) — every key with comments

## `roki.toml` schema

Per workspace, specified with `--config <path>` ([cli.md](cli.md)).

| Block / Key | Required | Meaning | Behavior on invalid value | Used by | Requirements |
|---|---|---|---|---|---|
| `[linear].api_token` (source) | yes | Where to fetch the Linear API token from (env / file / etc.) | Refuses startup if it cannot be resolved | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 2.3 |
| `[linear].webhook_secret` (source) | yes | Where to fetch the Linear webhook HMAC secret from | Refuses startup if it cannot be resolved | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 2.3, Req 3.1 |
| `[linear].assignee` | yes | Assignee to admit. `me` resolves to the API token holder | Refuses startup on resolution failure or multiple resolutions | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 2.8, Req 2.9 |
| `[linear].admit_states` | no | Set of Linear workflow state names to admit (default `["Todo"]`) | Refuses startup on empty set | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 2.11 |
| `[workflow].path` | yes | Path to `WORKFLOW.md` | Refuses startup if missing / unreadable | [02-configuration](../fr/02-configuration.md) | roki-mvp Req 2.4, Req 6.1 |
| `[server].bind` | no | Bind host of the webhook receiver (overridable via CLI `--bind`) | Refuses startup on bind failure | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 2.5 |
| `[server].port` | no | Bind port of the webhook receiver (overridable via CLI `--port`) | Refuses startup on bind failure | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 2.5 |
| `[[repos]].ghq` | 0+ | `ghq` identifier of an allowlisted repo (`owner/repo` or `host/owner/repo`) | Refuses startup on duplicates; an empty allowlist still boots (judge results route to `Skipped`) | [05-setup-judge](../fr/05-setup-judge.md), [06-worktree-and-session](../fr/06-worktree-and-session.md) | roki-mvp Req 2.1, Req 2.2, Req 2.7 |
| `[judge].model` | no | Claude model used by the setup judge | The documented default applies when omitted | [05-setup-judge](../fr/05-setup-judge.md) | roki-mvp Req 2.10 |
| `[notifications.slack]` | no | Webhook URL or bot token + target channel | Refuses startup if the block is present and the destination cannot be resolved; absence yields a warning + skip | [14-operator-notifications](../fr/14-operator-notifications.md) | roki-mvp Req 2.12 |
| `[permissions].strategy` | yes | `--settings` allowlist or `--dangerously-skip-permissions` (also overridable via CLI flag) | Refuses startup if not set | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 9.3, Req 9.4, Req 9.5 |

`roki.toml` itself is **not hot-reloaded** (a restart is required).

## `WORKFLOW.md` schema

Per workspace, Liquid + Markdown, hot-reload supported. Composed of front matter (YAML or TOML) and template blocks.

### Front matter / structure

| Key | Required | Meaning | Used by | Requirements |
|---|---|---|---|---|
| `prompt_template_setup` (named template block) | yes | Prompt block for the setup judge subprocess | [05-setup-judge](../fr/05-setup-judge.md) | roki-mvp Req 6.1, Req 6.6 |
| `prompt_template_worker` (named template block) | yes | Prompt block for the main worker subprocess | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 6.1, Req 6.6 |

### Reserved extension namespaces

Each downstream spec consumes only its own namespace. The loader **round-trips unknown keys** (does not interpret them, does not delete them).

| Namespace / Key | Consuming spec | Required | Meaning | Used by | Requirements |
|---|---|---|---|---|---|
| `extension.gates.spec.required_status` | roki-spec-gate | no | The Linear status the gate evaluates (logged when defaulted) | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 7.1, Req 7.3 |
| `extension.gates.spec.timeout_ms` | roki-spec-gate | no | Per-attempt timeout. Non-positive causes the gate evaluation for that repo to be refused | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 4.1, Req 7.5 |
| `extension.gates.spec.max_attempts` | roki-spec-gate | no | Attempt cap. Same as above for non-positive | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 4.3, Req 7.5 |
| `extension.gates.review.required_status` | roki-review-gate | no | The artifact status considered a pass (default `pass`) | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-review-gate Req 6.2 |
| `extension.gates.review.timeout_ms` | roki-review-gate | no | Upper bound on the review turn's duration | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-review-gate Req 6.3, Req 8.1 |
| `extension.gates.review.max_attempts` | roki-review-gate | no | Review attempt cap (default 3) | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-review-gate Req 5.1, Req 6.4 |
| `extension.server.port` | roki-observability | no | HTTP API port (omitting disables the API) | [15-http-api](../fr/15-http-api.md) | roki-observability Req 1.1, Req 1.2, Req 15.2 |
| `extension.server.bind` | roki-observability | no | HTTP API bind host (default `127.0.0.1`) | [15-http-api](../fr/15-http-api.md) | roki-observability Req 7.1, Req 15.2 |
| `extension.server.min_refresh_interval_seconds` | roki-observability | no | Minimum coalescing interval for `POST /refresh` | [15-http-api](../fr/15-http-api.md) | roki-observability Req 4.4, Req 15.2 |
| `extension.server.max_event_log_per_issue` | roki-observability | no | Maximum length of the event log returned by the per-issue endpoint | [15-http-api](../fr/15-http-api.md) | roki-observability Req 3.6, Req 15.2 |
| `extension.distill.paths` | roki-distill-postmerge | no | List of path patterns to sweep | [10-post-merge-distill](../fr/10-post-merge-distill.md) | roki-distill-postmerge Req 4.1, Req 4.3 |
| `extension.distill.routes` | roki-distill-postmerge | no | Classification rules (path/filename pattern → `delete`/`archive`/`distill`) | [10-post-merge-distill](../fr/10-post-merge-distill.md) | roki-distill-postmerge Req 4.2 |

### Hot reload and validation

- **Schema validation failure at startup** → refuse to start + log the offending key path
- **Validation passes on hot reload** → apply the new policy
- **Validation fails on hot reload** → **keep the previous policy** + log the failure (do not stop the daemon)
- **Per-key invalidity inside `extension.*`** (e.g. non-positive `timeout_ms`) → the corresponding spec refuses evaluation + logs the misconfiguration

## When adding a new key / namespace

1. Add a row to the corresponding table above (Block/Key / Required / Meaning / Used by / Requirements).
2. From the FR page that uses it, link to this table.
3. Update `roki-mvp Req 2` (for `roki.toml`) or `roki-mvp Req 6.5` (for a `WORKFLOW.md` namespace) and the consuming spec's requirements.

## Related

- [cli.md](cli.md): override via CLI flags
- [extension-surface.md](extension-surface.md): the extension contract including WORKFLOW.md namespaces
- [`docs/examples/`](../examples/): working samples (minimal + annotated)
