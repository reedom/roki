---
refs:
  id: fr:07-worker-execution
  kind: fr
  title: "Phase Subprocess Execution"
  spec: roki-mvp
  implements:
    - req:roki-mvp:5
    - req:roki-mvp:5.10
    - req:roki-mvp:9
  related:
    - fr:01-daemon-lifecycle
    - fr:02-configuration
    - fr:04-state-machine-and-recovery
    - fr:06-worktree-and-session
    - fr:14-operator-notifications
    - fr:20-rule-and-cycle-engine
    - fr:21-log-access
---

# FR 07: Phase Subprocess Execution

> The daemon-side mechanics that spawn each pre / run / post subprocess for a cycle, capture its stdout / stderr to disk, parse a structured directive from the last JSON object on stdout (pre / post), apply stall detection, and route the outcome back to the cycle engine. The cycle dispatch logic — which list to evaluate, what directive means what — lives in [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md); this FR is the daemon-side process supervisor only.

## Purpose

Run each phase the cycle engine nominates as a single bounded subprocess. The daemon observes the lifecycle (launch, stall, exit) and forwards a structured outcome to the cycle engine; it does not drive the agent loop, choose the next phase, or interpret reasoning text. There are two subprocess shapes — long-lived AI session and one-shot command — supervised by the same engine adapter.

The previous orchestrator-session-vs-phase-subprocess distinction is collapsed. Both shapes go through this FR's launch / observe / translate path. Permission strategy is no longer interpreted by the daemon: whatever the operator's cli line says (e.g. `claude --dangerously-skip-permissions`, `--settings` overrides, sandbox profile flags) is passed through unchanged. The daemon does not parse, validate, or override permission flags.

## User-visible Behavior

### Subprocess shapes

| Shape | Declared by | cli source | Lifetime | Stdin protocol |
|---|---|---|---|---|
| Session | `session: "session"` in workflow/*.md frontmatter, or any inline `*.prompt` field | `roki.toml [default.ai.session].cli` | Reused across all pre and post invocations of the **same cycle** (one process per cycle, terminated when the cycle ends) | Bidirectional stream-json: the daemon writes one event JSON per turn; the AI replies with one terminal JSON per turn |
| Command | `session: "command"` in workflow/*.md frontmatter, or any inline `*.cmd` field | `roki.toml [default.ai.command].cli`, or the workflow/*.md `cli` frontmatter, or the inline `*.cmd` string itself | Single invocation per phase iteration — fresh subprocess every time | Unidirectional: the daemon writes one launch envelope JSON to stdin and closes |

Run-phase subprocesses are typically command-shape (the run is the heavyweight code-changing subprocess and benefits from a fresh sandbox each time). Pre and post are typically session-shape so the AI keeps in-cycle reasoning state across iterations. Operators are free to mix and match.

### Launch and observation

**Trigger**: the cycle engine signals "spawn phase X with envelope E". The daemon does not pick what to spawn — it follows the engine's directive.

**Launch (session)**: at the start of a cycle, if any phase declares session-shape, the daemon spawns one subprocess running `[default.ai.session].cli` inside the issue's session tempdir, with a working directory set as documented in [06-worktree-and-session](06-worktree-and-session.md). The same process is reused across pre and post invocations of the cycle. The daemon writes a launch-envelope JSON to stdin once on first use, then writes one event JSON per turn after each cooperative `directive: "..."` reply.

**Launch (command)**: each invocation spawns a fresh subprocess. The daemon renders the cli line as a Liquid template (substituting `{{ pre.* }}` / `{{ post.* }}` / `{{ ticket.* }}` / `{{ cycle.* }}` per [20-rule-and-cycle-engine §Inter-phase data flow](20-rule-and-cycle-engine.md)), spawns the process, writes the envelope JSON to stdin, closes stdin, and waits for exit.

**Capture**: stdout and stderr are copied to per-iter files under `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{phase}.{stdout,stderr}` ([21-log-access §Storage layout](21-log-access.md)). The capture is byte-for-byte; the daemon does not strip ANSI codes or filter content.

**Event handling**: stdout is also parsed as it arrives, line-by-line:

- For session phases (pre / post) emitting stream-json: the daemon scans for the terminal JSON object that contains a `directive` field. Earlier JSON objects (advisory thinking blocks, tool-use messages, etc.) are recorded only in the iter capture file; they do not affect the cycle.
- For command phases (run, or pre/post in command shape): the daemon reads the entire stdout, then on exit parses the **last** JSON object (for pre / post) or the terminal `result` event ([21-log-access §`run.terminal.json`](21-log-access.md)) (for run, when the command speaks claude/codex stream-json).

### Permission strategy (pass-through)

Whatever the cli line says is what runs. The daemon does not enforce sandbox levels, allowlists, or `--dangerously-skip-permissions` decisions. Operators take responsibility for the resulting safety posture by choosing cli lines they trust:

```toml
[default.ai.session]
cli = "claude --input-format stream-json --output-format stream-json --model claude-opus-4-7 --settings ~/.config/roki/orchestrator.settings.json"

[default.ai.command]
cli = "claude -p '{{ prompt }}' --output-format stream-json --max-turns 100 --dangerously-skip-permissions"
```

The previous `[permissions].strategy` switch (`allowlist` vs `dangerously-skip` vs `refuse-to-start-when-unset`) is removed. Operators that want a fail-closed mode can omit `[default.ai.session]` / `[default.ai.command]` and rely on each rule's per-phase cli to be set explicitly.

### Termination handling

The daemon translates each phase's exit into a single signal returned to the cycle engine. The engine, not the daemon, decides what comes next.

| Outcome | When | Forwarded to engine |
|---|---|---|
| Clean directive | The terminal JSON object on stdout has a legal `directive` value for the phase | `{ kind: "directive", directive: <value>, payload: <parsed JSON> }` |
| Unparseable | No JSON object on stdout, or the last JSON object lacks `directive` | `{ kind: "failure", failure_kind: "unparseable" }` |
| Schema drift | `directive` value is outside the legal set for the phase | `{ kind: "failure", failure_kind: "schema_drift" }` |
| Process crash | Subprocess exited via signal or non-zero exit code without producing any directive | `{ kind: "failure", failure_kind: "process_crash", exit_code: N }` |
| Stall | Stdout silent for the configured stall window; daemon SIGTERMed the subprocess | `{ kind: "failure", failure_kind: "stall" }` |
| Iteration cap (cooperative) | The daemon wrote `iteration_exhausted` to the session's stdin and the AI replied with `directive: "end"` | `{ kind: "directive", directive: "end" }` (handled as ordinary clean termination) |
| Iteration cap (forced) | Same as above but the AI did not reply within the stall window | `{ kind: "failure", failure_kind: "iter_exhausted" }` |
| Template render error | Liquid render of the cli line, prompt body, or envelope failed before launch | `{ kind: "failure", failure_kind: "template_error" }` |

The cycle engine routes all `failure_kind` values through `[[on_failure]]` first-match ([20-rule-and-cycle-engine §Failure handling](20-rule-and-cycle-engine.md)). The previous daemon-side retry budget is removed: the daemon does not retry a phase on its own and does not enforce exponential backoff. Retries are operator-driven through post directives.

### Stall detection

Each subprocess has a stall window:

- Session shape: `roki.toml [default.ai.session].stall_seconds` (default `600`).
- Command shape: `roki.toml [default.ai.command].stall_seconds` (default `300`).
- Per-file override: workflow/*.md frontmatter `stall_seconds: <int>`.

The window is measured from the most recent stdout byte; if the subprocess emits a single byte every 100 ms it never stalls regardless of CPU work.

When the window elapses, the daemon sends SIGTERM, waits up to a fixed grace period (currently 10 s), then sends SIGKILL if the process is still alive. The captured stdout/stderr remain on disk.

### Tracker terminal handling

A Linear status change to `Done` / `Cancelled` (or assignee removal, or any other webhook content) does **not** preempt an in-flight cycle. The new state lands in the diff cache; rule re-evaluation defers until the cycle ends. Operators that want forced termination on a tracker terminal author a `[[cleanup]]` entry whose run phase issues whatever signal they want; the cleanup cycle starts only after the in-flight cycle completes.

### Daemon-only failures (no Linear writes)

The daemon never writes Linear directly. Failures detected by the daemon (stall, process crash, unparseable, schema drift, iteration cap, template error) flow through `[[on_failure]]`. If `[[on_failure]]` matches, the operator's failure-handler cycle decides whether to write Linear feedback. If `[[on_failure]]` does not match (or is absent), the failure is recorded in the structured event log and as one entry in the TUI escalation queue ([14-operator-notifications](14-operator-notifications.md)); no Linear write is attempted.

The previously specified linear-updater subagent and the `daemon_directive (kind=retry_exhausted)` event are removed. Their replacement is `[[on_failure]]` plus the operator's own Linear-write tooling inside the failure-handler cycle's run / post phase.

## Capabilities

- **One subprocess per phase invocation, two shapes**: session (one process per cycle, reused across pre/post turns) and command (one process per phase iteration). Both use the same engine adapter for capture, stall detection, and exit translation.
- **Pass-through cli lines**: the operator-authored cli is what runs. The daemon does not parse or rewrite permission flags.
- **Structured directive parsing**: pre / post terminal JSON object on stdout is the only "thinking" surface the daemon parses. Earlier output is captured but does not influence the cycle.
- **Engine-agnostic**: anything that follows the wire shape (stream-json bidirectional for session, exit-code + last-JSON-on-stdout for command) works. The daemon is not claude-specific.
- **Per-launch logging**: every subprocess launch records the phase, cli, env vars, working dir, and (on completion) outcome and exit code in the structured event log.
- **Stall handling**: per-shape default with per-file override. SIGTERM + grace + SIGKILL.
- **Operator-driven retry**: post directives `pre` / `run` are how the operator retries. The daemon does not retry on its own.
- **Failure routing**: every failure kind (process crash, unparseable, schema drift, stall, iter exhausted, template error) flows through `[[on_failure]]` first-match; default is structured log + escalation entry.

## Boundaries

- **Driving the agent loop** is owned by whatever cli the operator runs. The daemon does not step the agent.
- **Selecting which phase runs next** is owned by the cycle engine, not the daemon. The daemon never picks a phase on its own.
- **Per-tool-granularity permission policy** is out of scope. Whatever permission surface the operator's cli supports is what is enforced.
- **Container / VM isolation** is out of scope (the daemon depends on whatever sandbox the operator's cli supplies).
- **Parsing reasoning text** is out of scope. Only the terminal JSON object on stdout (for pre/post) and the terminal `result` event (for run, when applicable) are interpreted.
- **Linear writes from the daemon** are out of scope; the daemon never writes Linear directly. Operators that want Linear feedback inside a phase pass the appropriate MCP / CLI through their cli line.
- **Daemon-side retry budgets and exponential backoff** are out of scope; operators express retry / backoff in their post directive (e.g. by sleeping inside the subsequent pre).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Phase subprocesses for code-changing work"; Constraints > Engine ("Phase subprocess (short-lived, one per phase)" + "Orchestrator session (long-lived)") collapses into the two-shape model here.
- **Requirements**:
  - `req:roki-mvp:5`: Bounded Subprocess Adapters (the contract here covers both session and command shapes).
  - `req:roki-mvp:5.10`: Retry budget exhaustion → operators express via `[[on_failure]] when.kind = "iter_exhausted"`.
  - `req:roki-mvp:9`: Permission Strategy (collapses to pass-through; operator owns the safety posture by choosing cli lines).
- **Design**:
  - `Engine Adapter` section of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
- **Related FR**: [01-daemon-lifecycle](01-daemon-lifecycle.md), [04-state-machine-and-recovery](04-state-machine-and-recovery.md), [06-worktree-and-session](06-worktree-and-session.md), [14-operator-notifications](14-operator-notifications.md), [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md), [21-log-access](21-log-access.md).
