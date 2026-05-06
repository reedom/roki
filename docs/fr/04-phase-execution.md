---
refs:
  id: fr:04-phase-execution
  kind: fr
  title: "Phase Subprocess Execution"
  spec: roki-mvp
  implements:
    - req:roki-mvp:5
    - req:roki-mvp:5.10
    - req:roki-mvp:7
    - req:roki-mvp:9
  related:
    - fr:12-daemon-lifecycle
    - fr:02-configuration
    - fr:07-recovery
    - fr:05-worktree-and-session
    - fr:06-failure-handling
    - fr:01-engine-model
    - fr:09-log-access-cli
---

# FR 04: Phase Subprocess Execution

> The daemon-side mechanics that spawn each pre / run / post subprocess for a cycle, capture its stdout / stderr to disk, parse a structured directive from the last JSON object on stdout (pre / post), apply stall detection, and route the outcome back to the cycle engine. The cycle dispatch logic — which list to evaluate, what directive means what — lives in [01-engine-model](01-engine-model.md); this FR is the daemon-side process supervisor only.

## Purpose

Run each phase the cycle engine nominates as a single bounded subprocess. The daemon observes the lifecycle (launch, stall, exit) and forwards a structured outcome to the cycle engine; it does not drive the agent loop, choose the next phase, or interpret reasoning text. There are two subprocess shapes — long-lived AI session and one-shot command — supervised by the same engine adapter.

Both subprocess shapes go through this FR's launch / observe / translate path. Permission strategy is not interpreted by the daemon: whatever the operator's cli line says (e.g. `claude --dangerously-skip-permissions`, `--settings` overrides, sandbox profile flags) is passed through unchanged. The daemon does not parse, validate, or override permission flags.

## User-visible Behavior

### Subprocess shapes

| Shape | Declared by | cli source | Lifetime | Stdin protocol |
|---|---|---|---|---|
| Session | `session: "session"` in workflow/*.md frontmatter, or any inline `*.prompt` field | `roki.toml [default.ai.session].cli` | Reused across all pre and post invocations of the **same cycle** (one process per cycle, terminated when the cycle ends) | stdin stays open across the cycle; daemon writes the rendered phase body once per pre / post turn. The cli's own input flags (e.g. claude `--input-format stream-json`) decide how those bytes are parsed. The cli replies with one terminal JSON per turn on stdout. |
| Command | `session: "command"` in workflow/*.md frontmatter, or any inline `*.cmd` field | `roki.toml [default.ai.command].cli`, or the workflow/*.md `cli` frontmatter, or the inline `*.cmd` string itself | Single invocation per phase iteration — fresh subprocess every time | stdin gets the rendered phase body once and closes (or closes immediately for inline `*.cmd` phases). |

Run-phase subprocesses are typically command-shape (the run is the heavyweight code-changing subprocess and benefits from a fresh sandbox each time). Pre and post are typically session-shape so the AI keeps in-cycle reasoning state across iterations. Operators are free to mix and match.

### Launch and observation

**Trigger**: the cycle engine signals "spawn phase X for cycle iteration N". The daemon does not pick what to spawn — it follows the engine's directive.

**Working directory**: every phase subprocess (session and command) is launched with cwd set to the ticket's worktree when one exists, otherwise to the ticket's session tempdir. The daemon resolves the worktree via `ghq list -p` plus the `wt` worktree convention ([05-worktree-and-session](05-worktree-and-session.md)); operators do not need to write `cd "$(roki repo)"` inside their cli line.

#### Input channels

Daemon delivers per-phase input on three fixed channels:

| Channel | Content | Notes |
|---|---|---|
| **argv** | Liquid-rendered cli line | The cli line itself is a Liquid template; operators substitute any field from [01-engine-model §Inter-phase data flow](01-engine-model.md) with `{{ ... }}`. For `path` / `prompt` phases the cli line comes from `[default.ai.{session,command}].cli` (or the workflow/*.md `cli` frontmatter override). For inline `cmd` phases the cli line is the `cmd` string itself. |
| **env** | `ROKI_*` scalars | One env var per scalar entry in the data-flow table. Complex objects are not flattened. |
| **stdin** | Rendered phase body | `path` phases write the rendered Liquid body. Inline `prompt` phases write the rendered inline string. Inline `cmd` phases write nothing (stdin is closed immediately). |

For session-shape phases stdin stays open across the cycle and the daemon writes the rendered body of each pre / post turn as a separate write; the cli's own input format (`--input-format stream-json`, plain-text stdin, etc.) decides how the bytes are framed. For command-shape phases stdin is written once and closed.

Operators that need a complex prev-iter object not present as a Liquid variable read it through `roki log --stream response --iter -N` ([09-log-access-cli](09-log-access-cli.md)).

**Launch (session)**: at the start of a cycle, if any phase declares session-shape, the daemon Liquid-renders `[default.ai.session].cli` and spawns one subprocess with the working directory described above. The same process is reused across all pre and post invocations of the cycle. On every pre / post turn, the daemon writes the rendered Liquid body for that turn to stdin; stdin stays open until the cycle ends.

**Launch (command)**: each invocation spawns a fresh subprocess. The daemon (a) Liquid-renders the cli line, (b) exports the `ROKI_*` scalar env vars, (c) spawns the process with the working-directory rule above, (d) writes the rendered phase body to stdin (for `path` / `prompt` phases) or closes stdin immediately (for inline `cmd` phases), and (e) waits for exit.

**Capture**: stdout and stderr are copied byte-for-byte, as they arrive, to per-iter files under `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{phase}.{stdout,stderr}` ([09-log-access-cli §Storage layout](09-log-access-cli.md)). The daemon does not strip ANSI codes or filter content. Parsed-derivative files are written incrementally (described under §Event handling) rather than only at exit, so an in-flight phase already exposes its terminal directive to operator tooling the moment the agent emits it.

**Event handling**:

- **Session phases (pre / post in session shape)**: stream-json is read line-by-line. Each parseable JSON object is appended verbatim to `<phase>.events.jsonl` as it arrives (1 event per line). Advisory events (thinking blocks, tool-use messages, etc.) are kept only here; they do not affect the cycle. The first parseable object whose top-level shape carries a `directive` field is treated as the **terminal** for the iteration: the daemon writes it to `<phase>.response.json`, forwards it to the cycle engine, and stops scanning further events for that iteration. The session subprocess itself stays alive for the next pre / post turn (or until cycle end). Output that arrives after the terminal directive within the same turn is appended to `events.jsonl` for forensics but is not interpreted.
- **Command-form pre / post**: the daemon reads stdout to completion (or until the subprocess exits). On exit it parses the **last** JSON object on stdout, writes it to `<phase>.response.json`, and forwards to the engine. There is no `events.jsonl` for command-form phases — advisory output stays in `<phase>.stdout` only.
- **Run phase**: stdout is captured as bytes. When the cli speaks claude/codex stream-json, the daemon scans for the terminal `result` event mid-stream and writes `run.terminal.json` the moment it is identified (without waiting for exit). Other shapes leave `run.terminal.json` absent. The numeric exit code is written to `run.exit_code` after `wait()` returns, regardless of cli shape.
- **Parse failures**: when no terminal directive is observed for a session / command-form pre or post phase, the corresponding `<phase>.response.json` is not written and the engine is notified of `unparseable` (or `schema_drift` if the directive value is illegal). The raw stdout / stderr captures stay on disk for forensics.

### Tool boundary and permissions (pass-through)

The daemon never registers, proxies, or wraps any agent-side tool. Every phase subprocess sees exactly what the operator's cli line invokes — Linear MCP, git, gh, shell, language MCPs — verbatim. The daemon adds nothing, removes nothing, and composes no flags on the operator's behalf.

- **Subprocess tool surface**: equals the cli line's tool surface.
- **Linear writes**: originate only from inside a phase subprocess, through whatever MCP / CLI / HTTP client the operator's cli line exposes. The daemon process itself never writes Linear under any circumstance — including failure handling, cleanup, and restart recovery.
- **Permission flags**: `--dangerously-skip-permissions`, `--settings`, sandbox profile flags are passed through unchanged. The daemon does not parse, validate, or override them.
- **Secrets**: Linear API token, webhook secret, and operator-declared `roki.toml` secrets are never placed in prompt input, captures, environment variables given to phase subprocesses, or structured log entries. The redaction layer in [08-observability-logs](08-observability-logs.md) enforces this at log emit time.
- **Operator safety posture**: operators choose the cli line for each phase. A constrained allowlist or a permissive `--dangerously-skip-permissions` are equally accepted by the daemon.

```toml
[default.ai.session]
cli = "claude --input-format stream-json --output-format stream-json --model claude-opus-4-7 --settings ~/.config/roki/claude.settings.json"

[default.ai.command]
cli = "claude -p --output-format stream-json --max-turns 100 --dangerously-skip-permissions"
```

Operators that want a fail-closed mode omit `[default.ai.session]` / `[default.ai.command]` and require each rule's per-phase cli to be set explicitly.

### Termination handling

The daemon translates each phase's exit into a single signal returned to the cycle engine. The engine, not the daemon, decides what comes next.

| Outcome | When | Forwarded to engine |
|---|---|---|
| Clean directive | The terminal JSON object on stdout has a legal `directive` value for the phase | `{ kind: "directive", directive: <value>, payload: <parsed JSON> }` |
| Unparseable | No JSON object on stdout, or the last JSON object lacks `directive` | `{ kind: "failure", failure_kind: "unparseable" }` |
| Schema drift | `directive` value is outside the legal set for the phase | `{ kind: "failure", failure_kind: "schema_drift" }` |
| Repo mismatch | Pre's `repo` field does not match the admission-resolved repo | `{ kind: "failure", failure_kind: "repo_mismatch" }` |
| Filesystem error | Worktree create or session-tempdir setup failed before subprocess launch | `{ kind: "failure", failure_kind: "fs_poison" }` |
| Process crash | Subprocess exited via signal or non-zero exit code without producing any directive | `{ kind: "failure", failure_kind: "process_crash", exit_code: N }` |
| Stall | Stdout silent for the configured stall window; daemon SIGTERMed the subprocess | `{ kind: "failure", failure_kind: "stall" }` |
| Iteration cap | A post directive `pre` / `run` arrived while `cycle.iter == [engine].max_iterations`. Daemon refuses to start the next iteration; closes stdin for session-shape phases and waits for clean exit (SIGTERM after stall window if the subprocess does not exit) | `{ kind: "failure", failure_kind: "iter_exhausted" }` |
| Template render error | Liquid render of the cli line or phase body failed before launch | `{ kind: "failure", failure_kind: "template_error" }` |

The cycle engine routes all `failure_kind` values through `[[on_failure]]` first-match ([01-engine-model §Failure handling](01-engine-model.md)). The daemon does not retry a phase on its own and does not enforce exponential backoff; retries are operator-driven through post directives.

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

The daemon never writes Linear directly. Failures detected by the daemon (stall, process crash, unparseable, schema drift, iteration cap, template error) flow through `[[on_failure]]`. If `[[on_failure]]` matches, the operator's failure-handler cycle decides whether to write Linear feedback. If `[[on_failure]]` does not match (or is absent), the failure is recorded in the structured event log and as one entry in the TUI escalation queue ([06-failure-handling](06-failure-handling.md)); no Linear write is attempted.

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

- **Roadmap**: `roadmap.md` > Scope > In > "Phase subprocesses for code-changing work".
- **Requirements**:
  - `req:roki-mvp:5`: Bounded Subprocess Adapters (covers both session and command shapes).
  - `req:roki-mvp:5.10`: Retry budget exhaustion → operators express via `[[on_failure]] when.kind = "iter_exhausted"`.
  - `req:roki-mvp:7`: Agent Tooling Boundary (the daemon registers, proxies, or wraps no agent-side tool).
  - `req:roki-mvp:9`: Permission Strategy — pass-through; operator owns the safety posture by choosing cli lines.
- **Design**:
  - `Engine Adapter` section of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
- **Related FR**: [12-daemon-lifecycle](12-daemon-lifecycle.md), [07-recovery](07-recovery.md), [05-worktree-and-session](05-worktree-and-session.md), [06-failure-handling](06-failure-handling.md), [01-engine-model](01-engine-model.md), [09-log-access-cli](09-log-access-cli.md).
