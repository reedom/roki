---
refs:
  id: fr:04-state-execution
  kind: fr
  title: "State Subprocess Execution"
  spec: roki-skeleton
  related:
    - fr:12-daemon-lifecycle
    - fr:02-configuration
    - fr:07-recovery
    - fr:05-worktree-and-session
    - fr:06-failure-handling
    - fr:01-engine-model
    - fr:09-log-access-cli
---

# FR 04: State Subprocess Execution

> The daemon-side mechanics that spawn each state subprocess for a cycle, capture its stdout / stderr to disk, read the per-state sentinel directive file at exit, apply stall detection, and route the outcome back to the cycle engine. The cycle dispatch logic — which list to evaluate, what directive means what — lives in [01-engine-model](01-engine-model.md); this FR is the daemon-side process supervisor only.

## Purpose

Run each state the cycle engine nominates as a single bounded subprocess. The daemon observes the lifecycle (launch, stall, exit), reads the operator-written sentinel-file directive, and forwards a structured outcome to the cycle engine. It does not drive the agent loop, choose the next state, or interpret reasoning text.

Every state is command-shape: each visit spawns a fresh subprocess. There is no long-lived AI session shared across states or visits within a state. Operators that need conversational continuity drive it inside one state's process (e.g. one stream-json invocation that holds the conversation through tool use).

Permission strategy is not interpreted by the daemon: whatever the operator's cli line says (e.g. `claude --dangerously-skip-permissions`, `--settings` overrides, sandbox profile flags) is passed through unchanged.

## User-visible Behavior

### Subprocess shape

Every state launch is a fresh subprocess. cli source order:

1. State `run: "<inline cmd>"` — the inline string is the cli line itself.
2. State `uses: "<path>"` — body is loaded from `workflow/*.md`. cli comes from the file's frontmatter `cli:` if set, else `roki.toml [default.ai].cli`.

Stdin protocol:

- `uses:` states write the rendered Liquid body to stdin once, then close.
- `run:` states close stdin immediately. The cli line carries any input via argv or shell redirection.

### Launch and observation

**Trigger**: the cycle engine signals "run state `<id>` on visit `<n>` for cycle `<uuid>`". The daemon does not pick what to spawn — it follows the engine's request.

**Working directory**: every state subprocess is launched with cwd set to the worktree when one exists, otherwise to the **ghq base path** of the admission-resolved repo. The daemon resolves both via `ghq list -p` plus the `wt` worktree convention ([05-worktree-and-session](05-worktree-and-session.md)); operators do not need to write `cd "$(roki repo)"` inside their cli line.

The session tempdir is **not** used as a cwd — it is the daemon-owned log-capture root only ([05-worktree-and-session §Session tempdir](05-worktree-and-session.md)).

States that run with cwd at the ghq base must treat it as **read-only** — writing into the ghq base pollutes the operator's main checkout. The worktree is the only daemon-managed writable filesystem; any work that mutates files belongs to a state that runs after worktree materialization.

#### Input channels

The daemon delivers per-state input on three fixed channels:

| Channel | Content | Notes |
|---|---|---|
| **argv** | Liquid-rendered cli line | The cli line itself is a Liquid template; operators substitute any field from [01-engine-model §Inter-state data flow](01-engine-model.md) with `{{ ... }}`. |
| **env** | `ROKI_*` scalars | One env var per scalar entry in the data-flow table. Plus `ROKI_DIRECTIVE_PATH` pointing at this visit's sentinel file. Complex objects are not flattened. |
| **stdin** | Rendered state body | `uses:` states write the rendered Liquid body once and close. `run:` states close stdin immediately. |

States that need a complex earlier-state object not present as a Liquid variable read it through `roki log --cycle <id> --state <state_id> --stream stdout` ([09-log-access-cli](09-log-access-cli.md)).

**Launch sequence (every state, every visit)**:

1. Allocate sentinel path `<session_tempdir>/directives/<state_id>.<visit_n>.json`; create the parent directory. fs error here surfaces as `fs_poison`.
2. Liquid-render the cli line. Render error → `template_error`.
3. Liquid-render the state body (for `uses:` states). Render error → `template_error`.
4. Export `ROKI_*` scalar env vars + `ROKI_DIRECTIVE_PATH` pointing at the sentinel path.
5. Spawn the subprocess with cwd per the working-directory rule above.
6. Write the rendered body to stdin (`uses:`) or close stdin (`run:`).
7. Wait for exit. Stall detection runs concurrently (§Stall detection).

**Capture**: stdout and stderr are copied byte-for-byte, as they arrive, to per-visit files under `<session_root>/<ticket-id>/cycle-<uuid>/visit-<n>/<state_id>.{stdout,stderr}` ([09-log-access-cli §Storage layout](09-log-access-cli.md)). The daemon does not strip ANSI codes or filter content.

Parsed-derivative files written incrementally:

- `<state_id>.events.jsonl` — when the cli line emits stream-json (claude / codex), each parseable JSON event line is appended verbatim as it arrives. Advisory events (thinking blocks, tool-use messages) live only here; they do not affect the cycle.
- `<state_id>.terminal.json` — the parsed claude/codex stream-json `result` event when the cli emits one. Other shapes leave this absent.
- `<state_id>.exit_code` — the numeric exit code, written after `wait()` returns.
- `<state_id>.directive.json` — copy of the sentinel file at exit, when present.

The sentinel directive (control channel) is **not** parsed from stdout. The daemon reads `$ROKI_DIRECTIVE_PATH` after the subprocess exits.

#### Sentinel directive contract

The operator's subprocess writes a JSON object to `$ROKI_DIRECTIVE_PATH` before exit. Atomic write is the operator's responsibility (write to `<path>.tmp`, rename to `<path>`).

```json
{
  "directive": "<name>",
  "outcome": "<optional terminal-outcome override>",
  "<operator field>": "..."
}
```

| State at exit | Daemon behavior |
|---|---|
| Sentinel absent | exit code 0 → `on_done` edge; non-zero → `on_fail` edge |
| Sentinel present, valid JSON, `directive` matched | resolve directive name in `state.directives` ∪ built-in defaults; take resolved edge |
| Sentinel present, `directive` not in resolved set | `schema_drift` failure |
| Sentinel present, JSON parse fails or `directive` field missing | `unparseable` failure |
| Sentinel present, target is a terminal, payload has `outcome:` | terminal's outcome label is overridden for this cycle's terminal record |

Stdout and stderr are pure work output. The daemon does not parse them for control flow.

### Tool boundary and permissions (pass-through)

The daemon never registers, proxies, or wraps any agent-side tool. Every state subprocess sees exactly what the operator's cli line invokes — Linear MCP, git, gh, shell, language MCPs — verbatim. The daemon adds nothing, removes nothing, and composes no flags on the operator's behalf.

- **Subprocess tool surface**: equals the cli line's tool surface.
- **Linear writes**: originate only from inside a state subprocess, through whatever MCP / CLI / HTTP client the operator's cli line exposes. The daemon process itself never writes Linear.
- **Permission flags**: `--dangerously-skip-permissions`, `--settings`, sandbox profile flags are passed through unchanged.
- **Secrets**: Linear API token, webhook secret, and operator-declared `roki.toml` secrets are never placed in prompt input, captures, environment variables given to state subprocesses, or structured log entries. The redaction layer in [08-observability-logs](08-observability-logs.md) enforces this at log emit time.
- **Operator safety posture**: operators choose the cli line per state. A constrained allowlist or a permissive `--dangerously-skip-permissions` are equally accepted by the daemon.

```toml
[default.ai]
cli = "claude -p --output-format stream-json --max-turns 100 --settings ~/.config/roki/claude.settings.json"
```

Operators that want a fail-closed mode omit `[default.ai]` and require each state's `uses:` frontmatter `cli:` (or inline `run:`) to set the cli line explicitly.

### Termination handling

The daemon translates each state's exit into a single signal returned to the cycle engine. The engine, not the daemon, decides what comes next.

| Outcome | When | Forwarded to engine |
|---|---|---|
| Edge, on_done | Exit 0 + sentinel absent | `Edge { on_done, captures }` |
| Edge, directive | Exit 0 + sentinel present + directive resolves | `Edge { resolved_target, captures }` |
| Edge, on_fail | Exit ≠ 0 | `Edge { on_fail, captures }` |
| Unparseable | Sentinel present but JSON parse fails or `directive` field missing | `Failure { kind: unparseable }` |
| Schema drift | Sentinel `directive` not in `state.directives` ∪ built-in defaults | `Failure { kind: schema_drift }` |
| Filesystem error | Worktree create / session-tempdir / sentinel-dir setup failed before subprocess launch | `Failure { kind: fs_poison }` |
| Process crash | Subprocess killed by signal without sentinel write | `Failure { kind: process_crash, exit_code: N }` |
| Stall | Stdout silent for the configured stall window; daemon SIGTERMed the subprocess | `Failure { kind: stall }` |
| Template render error | Liquid render of cli line, body, or `if:` condition failed before launch | `Failure { kind: template_error }` |

The cycle engine routes all `Failure` outcomes through `on_failure:` first-match ([01-engine-model §Failure handling](01-engine-model.md)). State-local on_done / on_fail edges stay inside the same cycle and do not trigger failure-cycle routing.

### Stall detection

Each subprocess has a stall window:

- Default: `roki.toml [default.ai].stall_seconds`.
- Per-file override: `workflow/*.md` frontmatter `stall_seconds: <int>`.
- Per-state override: `state.timeout` in WORKFLOW.yaml.

Canonical defaults and validation rules live in [`docs/reference/config.md`](../reference/config.md).

The window is measured from the most recent stdout byte; if the subprocess emits a single byte every 100 ms it never stalls regardless of CPU work.

When the window elapses, the daemon sends SIGTERM, waits up to a fixed grace period, then sends SIGKILL if the process is still alive. The captured stdout / stderr remain on disk.

### Tracker terminal handling

A Linear status change to `Done` / `Cancelled` (or assignee removal, or any other webhook content) does **not** preempt an in-flight cycle. The new state lands in the diff cache; rule re-evaluation defers until the cycle ends. Operators that want forced termination on a tracker terminal author a `cleanup:` entry whose state body issues whatever signal they want; the cleanup cycle starts only after the in-flight cycle completes.

### Daemon-only failures (no Linear writes)

The daemon never writes Linear directly. Failures detected by the daemon (`process_crash`, `unparseable`, `schema_drift`, `fs_poison`, `stall`, `recursion_bound`, `template_error`) flow through `on_failure:` first-match. If a handler matches, the operator's failure-handler cycle decides whether to write Linear feedback. If no handler matches (or `on_failure:` is absent), the daemon emits a `failure_unhandled` structured event ([06-failure-handling §Failure-handler cycle](06-failure-handling.md)) and retains the worktree for forensics. The escalation queue is **not** touched in this case (it is reserved for daemon-stuck cases per [06-failure-handling §Escalation queue](06-failure-handling.md)).

## Capabilities

- **One subprocess per state visit**: the engine adapter handles capture, stall detection, and exit translation uniformly. No long-lived session shape.
- **Pass-through cli lines**: the operator-authored cli is what runs. The daemon does not parse or rewrite permission flags.
- **Sentinel-file control channel**: directive lives in `$ROKI_DIRECTIVE_PATH`. Stdout / stderr are pure work output.
- **Engine-agnostic**: any cli that exits with a code and (optionally) writes a sentinel JSON object before exit works. Stream-json is one supported event surface among many; the daemon imposes no specific stdout wire format.
- **Per-launch logging**: every subprocess launch records `state_id`, `visit_n`, cli, env vars, working dir, and (on completion) outcome and exit code in the structured event log ([ref:log-events](../reference/log-events.md)).
- **Stall handling**: default + per-file + per-state override. SIGTERM + grace + SIGKILL.
- **Operator-driven retry**: directives bound to self-loops (`retry: <self>` by default) are how the operator retries. The daemon does not retry on its own; `max_visits` is the only daemon-enforced cap.
- **Failure routing**: every failure kind flows through `on_failure:` first-match; default on no-match is a `failure_unhandled` structured event (no escalation entry — the queue is reserved for daemon-stuck cases per [06-failure-handling §Escalation queue](06-failure-handling.md)).

## Boundaries

- **Driving the agent loop** is owned by whatever cli the operator runs. The daemon does not step the agent.
- **Selecting which state runs next** is owned by the cycle engine, not the daemon. The daemon never picks a state on its own.
- **Long-lived AI session across visits** is out of scope. Every state visit spawns a fresh subprocess.
- **Per-tool-granularity permission policy** is out of scope. Whatever permission surface the operator's cli supports is what is enforced.
- **Container / VM isolation** is out of scope (the daemon depends on whatever sandbox the operator's cli supplies).
- **Parsing reasoning text** is out of scope. Only the sentinel JSON file at `$ROKI_DIRECTIVE_PATH` and the optional terminal `result` event in `<state_id>.terminal.json` are interpreted.
- **Linear writes from the daemon** are out of scope; the daemon never writes Linear directly. Operators that want Linear feedback inside a state pass the appropriate MCP / CLI through their cli line.
- **Daemon-side retry budgets and exponential backoff** are out of scope; operators express retry / backoff in their state body (e.g. by sleeping inside the next state in a self-loop).

## Related

[12-daemon-lifecycle](12-daemon-lifecycle.md), [07-recovery](07-recovery.md), [05-worktree-and-session](05-worktree-and-session.md), [06-failure-handling](06-failure-handling.md), [01-engine-model](01-engine-model.md), [09-log-access-cli](09-log-access-cli.md).
