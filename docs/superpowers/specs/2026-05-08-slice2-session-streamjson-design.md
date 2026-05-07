# Slice 2 — Session-Shape & Stream-JSON Design

Date: 2026-05-08
Scope: Layer the long-lived session-shape subprocess, stream-json line-by-line parsing, stall detection (SIGTERM + grace + SIGKILL), per-file `stall_seconds` override, and `run.terminal.json` extraction on top of slice 1's command-shape engine. After this slice the daemon can drive a cycle whose pre/post phases reuse a single AI subprocess across iterations, while the run phase still spawns command-shape per iteration but exposes claude/codex stream-json `result` events to the post template via `{{ run.terminal.* }}`.

## 1. Position in the Roadmap

Slice 2 lifts four roadmap surfaces deferred by slice 1:

- `roki-engine-session-shape` — `fr:01` / `fr:04` long-lived subprocess across pre/post turns.
- `roki-engine-stream-json` — `fr:04 §Capture` incremental events.jsonl + response.json.
- `roki-engine-stall` — `fr:01 §Stall detection` / `fr:04 §Stall detection` SIGTERM + grace + SIGKILL.
- `roki-engine-run-terminal` — `fr:04 §Capture` claude/codex `result` event extraction to `run.terminal.json`.

Slice 1 (`docs/superpowers/specs/2026-05-07-slice1-engine-mvp-design.md`) provides: directive-driven `pre → run → post` loop, `[engine].max_iterations` cap, Liquid render, `ROKI_*` env, command-shape `CommandPhaseExecutor`, `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/` capture layout, and config-load rejection of `session = "session"`. This slice removes the rejection, adds the session call path, and extends both shapes with stall detection.

Out-of-scope, deferred to later slices:

- `[[on_failure]]` first-match cycle and the escalation queue. Failures still terminate the binary with exit 1.
- `[[cleanup]]` cycles.
- Worktree creation via `wt`. Session cwd stays at the ghq base path.
- Persistent daemon lifecycle, queue preemption, cold-start enumeration, hot reload.
- Per-repo `WORKFLOW.toml` overrides.
- `run`-phase session shape. Slice 2 rejects `[[rule.run]]` with `session = "session"` at config load. The FR convention treats run as command-shape; lifting this restriction is a separate later slice.

## 2. Architecture

### 2.1 Module layout

```
crates/roki-daemon/src/engine/
├── mod.rs              // re-exports: run_cycle, PhaseExecutor, SessionSupervisor, CycleOutcome, FailureKind, PhaseKind, PhaseShape
├── outcome.rs          // PhaseBody gains PhaseShape + stall_seconds; FailureKind gains Stall
├── cycle.rs            // dispatches per phase: command → CommandPhaseExecutor, session → SessionSupervisor
├── phase.rs            // CommandPhaseExecutor (slice 1) + run.terminal.json scanner + stall watchdog
├── session.rs          // NEW: SessionSupervisor (long-lived child + reader task + parser + events.jsonl writer + iter_exhausted shutdown)
├── stream.rs           // NEW: line splitter (BufReader::lines on stdout pipe) + claude/codex result-event recognizer
├── stall.rs            // NEW: idle-watchdog (last-stdout-byte AtomicU64 + SIGTERM/grace/SIGKILL helper)
├── directive.rs        // existing scan_last_json_object + new streaming variant for session phases
├── template.rs         // unchanged (slice 1)
└── context.rs          // PhaseContext gains optional run.terminal: Value
```

`SessionSupervisor` is owned by `engine::cycle::run_cycle` for the lifetime of a cycle. Construction is lazy: the supervisor is only created when at least one of `rule.pre` / `rule.post` declares `shape == Session`. A pure-command cycle uses zero new code paths.

`CommandPhaseExecutor` and `SessionSupervisor` produce the same `PhaseOutcome` / `PhaseInfraError` types so `engine::cycle::run_cycle` can switch on shape without further translation.

### 2.2 Call graph

```
runtime::run_inner
  └─ engine::cycle::run_cycle(ticket, matched_rule, &cfg, session_root)
      ├─ if any phase has shape Session:
      │     supervisor = SessionSupervisor::spawn(&cfg.default.ai.session, &ctx, cycle_root)?
      ├─ for iter in 1..=max_iterations:
      │     iter_dir = create_iter_dir(...)
      │     pre  → command_executor.execute()  OR  supervisor.run_turn(Pre, ...)
      │     run  → command_executor.execute()                         // run is always command-shape (slice 2)
      │     post → command_executor.execute()  OR  supervisor.run_turn(Post, ...)
      └─ supervisor.shutdown(reason: Completed | IterExhausted | Failed)
```

Cycle outcome unchanged from slice 1:

```rust
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed    { kind: FailureKind, iter: u32 },
}
```

### 2.3 Crate split

Stays inside `crates/roki-daemon`. No `roki-engine` extraction. The session supervisor is small enough that the trade-off from slice 1 §2.2 still applies.

## 3. Phase shape and config schema

### 3.1 `PhaseShape`

```rust
pub enum PhaseShape { Session, Command }
```

Per-phase shape is determined at config load:

| Body form              | Default shape | Override via                               | Notes                                       |
| ---------------------- | ------------- | ------------------------------------------ | ------------------------------------------- |
| inline `cmd = "..."`   | `Command`     | n/a — `session = "*"` is a load error      | inline cmd has its own argv; reusing across turns is meaningless |
| inline `prompt = "..."`| `Session`     | `session = "command"` flips to one-shot   | matches `fr:04 §Subprocess shapes`           |
| `path = "..."`         | `Command`     | `session = "session"` flips to long-lived | matches `fr:04 §Subprocess shapes`           |

Run-phase body must resolve to `PhaseShape::Command`. Slice 2 rejects `[[rule.run]] session = "session"` at load with `WorkflowError::SessionRunUnsupported` (deferred surface). Pre and post may be either shape independently of each other.

### 3.2 `PhaseBody`

```rust
pub enum PhaseBody {
    InlineCmd    { cmd: String,             stall_seconds: Option<u32> },
    InlinePrompt { prompt: String,          shape: PhaseShape, stall_seconds: Option<u32> },
    Path         { path: PathBuf, cli: Option<String>, shape: PhaseShape, stall_seconds: Option<u32> },
}
```

`stall_seconds: Option<u32>` is a per-file override. When `None`, the phase falls back to `[default.ai.{session,command}].stall_seconds` resolved by shape at execution time. Validated at load: must be `>= 1` if present.

Slice 1's `Rule { pre: Option<PhaseBody>, run: PhaseBody, post: Option<PhaseBody> }` is unchanged structurally; only the variant payloads change. The slice-1 invariant that `run` is `PhaseBody::*` (any variant) still holds — the new shape check happens after parse, in `Rule::validate`.

### 3.3 `[default.ai.session]`

```toml
[default.ai.session]
cli           = "claude --input-format stream-json --output-format stream-json --model claude-opus-4-7"
stall_seconds = 600   # default per docs/reference/config.md
```

Loader applies `stall_seconds = 600` if absent; validates `>= 1`. `[default.ai.command]` (slice 1) keeps its 300-second default.

`cli` is required when any phase resolves to `PhaseShape::Session`; missing-cli is reported as `ConfigError::MissingSessionCli` at cycle start (not config load), because the shape resolution depends on which `[[rule]]` matches at admission time.

## 4. SessionSupervisor

### 4.1 Lifecycle

```
SessionSupervisor::spawn(session_cfg, ctx, cycle_root)
  ├─ render argv from session_cfg.cli via Liquid (uses ctx at cycle start; cycle.iter = 0)
  ├─ shell-words split argv
  ├─ Command::new(argv[0]).args(...).env_clear().envs(ROKI_* + PATH/HOME/USER passthrough)
  │     .current_dir(ghq_base)
  │     .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
  │     .spawn()
  ├─ start stdout reader task → events MPSC channel
  ├─ start stderr drain task → cycle_root/<phase>.stderr per turn (see §4.4)
  └─ start stall watchdog task → SIGTERM/grace/SIGKILL on idle

run_turn(kind, body, ctx, iter_dir):
  ├─ render body string via Liquid (kind ∈ {Pre, Post})
  ├─ open iter_dir/<phase>.events.jsonl (append) and iter_dir/<phase>.stdout (write) for this turn
  ├─ activate the per-turn writer in the reader task (atomic swap; previous turn's writer is closed)
  ├─ write rendered body bytes to child.stdin (do NOT close)
  ├─ await directive event from channel (or stall, or process exit)
  │     directive arrives → write iter_dir/<phase>.response.json; return PreDirective | PostDirective + payload
  │     stall            → return Failure(Stall)
  │     unexpected exit  → return Failure(ProcessCrash)
  │     channel closed   → return Failure(Unparseable)
  └─ on success: turn-scoped events.jsonl/stdout files stay open until shutdown, lines after the directive are still appended for forensics

shutdown(reason):
  ├─ close child.stdin (drop the OwnedWriteHalf)
  ├─ wait up to stall_seconds for clean exit
  │     (timer source: same idle-watchdog clock; reset on stdin close)
  ├─ if still alive: SIGTERM
  ├─ wait fixed grace (5 s constant: GRACE_PERIOD)
  ├─ if still alive: SIGKILL
  └─ join reader / stderr-drain / stall-watchdog tasks
```

Reason values:

- `Completed` — cycle ended via terminal directive. stdin close is sufficient on a well-behaved CLI; SIGTERM is the safety valve.
- `IterExhausted` — post returned `pre`/`run` at `iter == max_iterations`. Same shutdown sequence; `FailureKind` is `IterExhausted`, not `Stall`, even if SIGKILL fires (per `fr:01 §125`).
- `Failed { kind }` — earlier failure on a phase. Shutdown still runs to free the process; the cycle outcome carries the original kind.

### 4.2 Stdout reader task

```
reader_task(stdout: ChildStdout, events_writer: Arc<Mutex<Option<FileHandles>>>, dir_chan: Sender<Event>):
  reader = BufReader::new(stdout)
  loop {
    line = reader.read_line()?              // \n-terminated; partial lines block until newline or EOF
    if EOF: break
    update last_stdout_byte (AtomicU64 = monotonic millis since spawn)
    parse JSON:
        Ok(Value)  → write line to events.jsonl (writer mutex)
                    if !directive_sent_for_this_turn AND value["directive"] is legal for current_kind:
                        write iter_dir/<phase>.response.json (pretty)
                        send Event::Directive { value } on dir_chan
                        directive_sent_for_this_turn = true
        Err(_)     → write line to events.jsonl as advisory (raw bytes); do not parse further
    if directive_sent: lines after still go to events.jsonl until the next turn swap
  }
  send Event::Exit on dir_chan
```

Line splitter: `tokio::io::BufReader::lines()` over `ChildStdout`. claude/codex stream-json is documented as one JSON object per line; very long lines (>64 KiB by default in tokio) are accepted because `BufReader::lines()` does not impose a line-length cap.

`current_kind` and `directive_sent_for_this_turn` are per-turn state held in a `tokio::sync::watch` channel that `run_turn` toggles atomically before writing to stdin. The reader task observes the latest value via `borrow()`.

### 4.3 Directive parsing

Per-phase legal set is identical to slice 1:

- `Pre`  ∈ `{run, end}`
- `Post` ∈ `{pre, run, end}`

A directive value outside the legal set is `Failure(SchemaDrift)`. A line that parses but lacks `directive` is appended to events.jsonl and ignored — the reader keeps scanning. If the channel closes (process exits) before any legal directive is sent, the turn returns `Failure(Unparseable)`.

### 4.4 Stderr handling

Stderr is drained on its own task to prevent OS-level pipe-fill stalls. Writes are appended to a per-turn file `iter_dir/<phase>.stderr` opened by `run_turn` and rotated at the next turn (same atomic-swap mechanism as stdout's `events.jsonl`). Bytes that arrive between turns (extremely rare with stream-json CLIs) are written to a fallback `cycle-<uuid>/session.stderr` so they are not silently dropped.

### 4.5 Concurrency

- The supervisor uses `Arc<Mutex<>>` for the per-turn file writers so the reader and stderr-drain tasks can swap them atomically.
- The directive channel is `tokio::sync::mpsc::channel(8)`. A buffer of 8 is enough to absorb advisory events that arrive after a terminal directive but before `run_turn` has fully returned.
- The stall watchdog uses an `Arc<AtomicU64>` for the "last stdout byte" timestamp; the reader task updates it on every line, the watchdog reads it on a 250 ms tick.

## 5. Stall detection

### 5.1 Common watchdog

A single `stall::Watchdog` struct is reused by `CommandPhaseExecutor` (per phase invocation) and `SessionSupervisor` (per cycle). It exposes:

```rust
impl Watchdog {
    pub fn new(stall_seconds: u32) -> Self;
    pub fn tick_stdout(&self);              // reader task calls on every byte
    pub async fn run(&self, child: ChildHandle) -> StallOutcome;
}

pub enum StallOutcome { Healthy, StalledThenTerminated }
```

Implementation:

```
run loop (250 ms interval):
  if now - last_stdout_byte > stall_seconds * 1000:
      child.signal(SIGTERM)
      wait up to GRACE_PERIOD (5 s constant)
      if !child.exited():
          child.signal(SIGKILL)
      return StalledThenTerminated
  if child.exited(): return Healthy
```

`GRACE_PERIOD` is a private `const Duration::from_secs(5)`; not surface-configurable. Justified by `fr:04 §126` ("waits up to a fixed grace period"). Operators control the user-facing tuning knob via `stall_seconds`.

### 5.2 Command-shape integration

`CommandPhaseExecutor::execute` spawns the child as in slice 1 then runs the watchdog concurrently with `child.wait()` via `tokio::select!`. On `StalledThenTerminated`, it returns `Failure(Stall)`. The captured stdout/stderr files remain on disk per `fr:04 §126`.

### 5.3 Session-shape integration

`SessionSupervisor` constructs one watchdog at spawn time using `[default.ai.session].stall_seconds`. The watchdog ticks on every line the reader task observes, regardless of which turn the line belongs to. `run_turn` mutates the watchdog window per §5.4 when the active phase carries a `PhaseBody.stall_seconds` override.

When stall fires mid-turn, the watchdog terminates the child; the directive channel receives `Event::Exit` and `run_turn` returns `Failure(Stall)`. The cycle is marked failed; `shutdown(Failed{Stall})` is a no-op for the child but still joins the reader / drain tasks.

When stall fires between turns (the supervisor sits idle waiting for the next `run_turn` call), the watchdog still terminates the child; the next `run_turn` invocation immediately observes `Event::Exit` and returns `Failure(ProcessCrash)`. This case is rare in practice because `engine::cycle::run_cycle` calls `run_turn` immediately after the previous turn returns.

### 5.4 Per-turn stall override

`PhaseBody.stall_seconds` overrides the shape default for that turn only. Implementation: `SessionSupervisor::run_turn` updates the watchdog's `stall_seconds` field via an `AtomicU32` swap before writing to stdin, and reverts to the supervisor default after the turn returns.

## 6. Stream-JSON capture

### 6.1 Session phase capture layout

```
<session_root>/
  <ticket-id>/
    cycle-<uuid>/
      iter-1/
        pre.stdout         # raw bytes from session subprocess during the pre turn
        pre.stderr         # raw bytes from session subprocess during the pre turn
        pre.events.jsonl   # one parsed JSON object per line; advisory + terminal both included
        pre.response.json  # pretty-printed terminal directive object
        run.stdout  run.stderr  run.exit_code  run.terminal.json  # run still command-shape
        post.stdout  post.stderr  post.events.jsonl  post.response.json
      iter-2/ ...
      session.stderr       # fallback for stderr bytes that arrive between turns (usually empty)
```

### 6.2 events.jsonl write semantics

- One line per JSON object. Object is the verbatim line text from stdout (no re-serialisation). Reason: preserves the CLI's exact wire formatting for debugging.
- Lines that fail to parse as JSON are still appended (the bytes are part of the agent's output and operators want forensics).
- Append-only; never rewritten.

### 6.3 response.json write semantics

- Written via `write_response_json` (slice 1 helper) once per turn, the moment the terminal directive is identified.
- Pretty-printed via `serde_json::to_string_pretty`.
- Overwritten if the same iter / phase fires twice (cannot happen in slice 2's loop, but the helper does not assume).

### 6.4 capture.rs surface change

```rust
pub fn open_session_phase_files(
    iter_dir: &Path,
    phase: PhaseKind,
) -> Result<SessionPhaseFiles, CaptureError>;

pub struct SessionPhaseFiles {
    pub stdout:  File,    // append for the duration of the turn
    pub stderr:  File,    // append for the duration of the turn
    pub events:  File,    // append; closed at next turn swap
}

pub fn open_session_fallback_stderr(cycle_root: &Path) -> Result<File, CaptureError>;
```

Slice 1's `open_phase_files`, `write_response_json`, `write_run_exit_code` are kept verbatim. The session helpers live alongside them.

## 7. run.terminal.json

### 7.1 Shape recognized

claude / codex stream-json emits a JSON object per line on stdout with a `type` field. The terminal event has `type == "result"` (claude) or `type == "result"` (codex stream-json — same shape). The recognizer treats them uniformly:

```json
{
  "type": "result",
  "subtype": "success" | "error_max_turns" | "error_during_execution",
  "result": "<final assistant text>",
  "is_error": false,
  "duration_ms": 12345,
  "duration_api_ms": 11000,
  "num_turns": 3,
  "session_id": "uuid",
  "total_cost_usd": 0.01234,
  "usage": { ... }
}
```

The daemon does not validate the inner schema beyond `type == "result"`. The full object is written verbatim (pretty-printed) to `iter-N/run.terminal.json`. Operators consume any field via `{{ run.terminal.<key> }}` in the post template.

### 7.2 Mid-stream extraction

`CommandPhaseExecutor::execute` runs the child with stdout teed: bytes are written to `run.stdout` and concurrently scanned line-by-line by `stream::scan_for_result`. On the first line whose parsed JSON satisfies `obj["type"] == "result"`, the scanner writes `run.terminal.json` immediately and stops scanning further lines (later lines still go to `run.stdout`). If the child exits without emitting a `result` event, `run.terminal.json` is not written. The `run.exit_code` write at exit is unchanged from slice 1.

Tee implementation:

```
spawn:
  Command::new(argv[0]).args(...).stdout(Stdio::piped()).stderr(...).spawn()

stdout_task:
  reader = BufReader::new(child.stdout)
  raw_writer = File::create(iter_dir/run.stdout)
  loop {
    line = reader.read_line()?
    if EOF: break
    raw_writer.write_all(line.as_bytes())?
    if !terminal_written:
        if let Ok(v) = serde_json::from_str(&line):
            if v["type"] == "result":
                write_pretty(iter_dir/run.terminal.json, &v)?
                terminal_written = true
  }
```

This replaces slice 1's `Command::stdout(File::create(...))` direct redirection. The `Watchdog::tick_stdout` call lives inside `stdout_task` so stall detection benefits from the same byte-by-byte signal.

### 7.3 PhaseContext extension

```rust
pub struct PhaseContext {
    // ...slice 1 fields...
    run_terminal: Option<serde_json::Value>,   // Some(v) iff iter-N/run.terminal.json was written
}

pub struct RunView {
    pub exit_code: i32,
    pub duration_seconds: u64,
    pub terminal: Option<serde_json::Value>,   // exposed as {{ run.terminal.* }}
}
```

`set_run` is extended to accept the terminal value (or `None`). `run_terminal` is cleared at the top of each iteration so a previous iter's value cannot leak.

`{{ run.terminal.* }}` is reachable from Liquid only. No `ROKI_RUN_TERMINAL_*` env export — the value can be deeply nested and the `ROKI_PRE_*` / `ROKI_POST_*` flattening rule (top-level scalars only) does not generalize. Operators that need a scalar in env render it explicitly via `cmd = "FOO={{ run.terminal.is_error }} my-script"`.

## 8. Failure routing

Slice 2 keeps slice 1's "failure → exit 1" policy because `[[on_failure]]` is deferred. The `FailureKind` enum gains:

```rust
pub enum FailureKind {
    Unparseable,
    SchemaDrift,
    ProcessCrash,
    TemplateError,
    IterExhausted,
    Stall,                  // NEW: stall_seconds exceeded (any shape)
    SessionSpawn,           // NEW: SessionSupervisor::spawn failed (cli missing, exec error)
}
```

| Trigger                                                                | Surface                                          | Exit |
| ---------------------------------------------------------------------- | ------------------------------------------------ | ---- |
| Any shape: stall_seconds exceeded; SIGTERM+grace+SIGKILL fired         | `PhaseOutcome::Failure(Stall)`                   | 1    |
| Session spawn fails (missing `[default.ai.session].cli`, exec error)   | `PhaseInfraError::SessionSpawn` → `SkeletonError`| 1    |
| Session subprocess exits before any directive in a turn                | `PhaseOutcome::Failure(ProcessCrash)`            | 1    |
| Session subprocess emits a directive value outside the legal set       | `PhaseOutcome::Failure(SchemaDrift)`             | 1    |
| Run command-shape: stdout has `type == "result"` line that fails to parse | not a failure — `run.terminal.json` is simply absent | n/a |
| All slice-1 failure rows                                               | unchanged                                        | 1    |

`iter_exhausted` semantics on session-shape match `fr:01 §123–125`: close stdin → wait stall window → SIGTERM → grace → SIGKILL. Outcome is `IterExhausted`, not `Stall`.

## 9. Templating context additions

```rust
ctx.run.terminal: Option<serde_json::Value>     // from iter-N/run.terminal.json, parsed lazily by set_run
```

Liquid object rebuilt at the start of each phase from `serde_json::to_value(&ctx)` (slice 1 mechanism). When `run_terminal` is `None`, Liquid sees a missing variable and renders empty (`{{ run.terminal.is_error }}` → empty string). Operators that need a default use Liquid's `default` filter: `{{ run.terminal.is_error | default: "false" }}`.

`fr:01 §Inter-phase data flow` table addition:

| Liquid variable           | Env var | Notes                                                   |
| ------------------------- | ------- | ------------------------------------------------------- |
| `{{ run.terminal.* }}`    | (Liquid only) | Parsed `result` event for the iter; null otherwise. |

## 10. Error handling

### 10.1 PhaseInfraError additions

```rust
pub enum PhaseInfraError {
    // slice 1 variants...
    SessionSpawn { cli: String, source: io::Error },
    SessionStdinClosed,            // child stdin already gone when run_turn writes
    SessionStdoutClosed,           // reader task observed EOF unexpectedly
    SessionRunUnsupported,         // load-time check; surfaced as ConfigError aggregate
    Stall,                         // bubbles up if the watchdog cannot find the child to signal
}
```

`SessionStdinClosed` and `SessionStdoutClosed` are infra-level (not `PhaseOutcome::Failure`). They map to exit 1 via the `SkeletonError` aggregator.

### 10.2 Forensics

On every failure the per-iter directory is left on disk. Additionally, the full `cycle-<uuid>/iter-*/<phase>.events.jsonl` chain is preserved unchanged so operators can replay exactly what the AI emitted, including the bytes after the terminal directive.

### 10.3 Structured events

Slice 2 adds:

- `session_spawned`         — fields: `cycle.id`, `cli` (head, truncated), cwd, pid.
- `session_turn_started`    — fields: `cycle.id`, `cycle.iter`, `phase` ∈ {`pre`,`post`}, body bytes (length only).
- `session_turn_completed`  — fields: `cycle.id`, `cycle.iter`, `phase`, `directive`, duration_ms.
- `phase_stalled`           — fields: `cycle.id`, `cycle.iter`, `phase`, `shape`, `stall_seconds`, signal_path (`SIGTERM` | `SIGKILL_after_grace`).
- `session_shutdown`        — fields: `cycle.id`, reason (`completed`|`iter_exhausted`|`failed`), exit_path (`stdin_close`|`sigterm`|`sigkill`), duration_ms.

Slice 1's `phase_started` / `phase_completed` / `phase_failed` are reused for both shapes (the existing `phase` field disambiguates). `phase_completed` gains an optional `terminal_kind` field for run phases that wrote `run.terminal.json` (value: `"result"`); other run phases omit it.

## 11. Testing strategy

### 11.1 Unit tests

| Module                | Coverage                                                                                                                        |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `engine::stream`      | Line splitter on partial UTF-8, very long lines (64 KiB+), windows-style `\r\n`, claude `result` recognition (positive + negative on missing `type`, wrong `type`, malformed JSON), codex same shape |
| `engine::stall`       | Deterministic clock via `tokio::time::pause`; simulated child writes; assert SIGTERM at `stall_seconds`, SIGKILL at `stall_seconds + GRACE_PERIOD`, healthy path on continuous bytes |
| `engine::session`     | Fake child via `tokio::io::duplex`; two-turn directive flow; advisory event before terminal; advisory event after terminal stays in events.jsonl; stdin write to closed child surfaces `SessionStdinClosed`; iter_exhausted shutdown sequence; stall during turn produces `Failure(Stall)`; stall between turns produces next-turn `Failure(ProcessCrash)` |
| `engine::directive`   | New streaming variant: pick first legal directive, ignore subsequent, ignore lines without `directive`                          |
| `engine::cycle`       | Mixed-shape transitions (pre=session, run=command, post=session) via fakes; pure-session-pre-and-post; pure-command (slice 1 regression) |
| `engine::phase`       | Run-phase tee: terminal-written-mid-stream, terminal-not-written (no `result` event), terminal `type` mismatch                  |
| `config::workflow`    | `session = "session"` accepted on `prompt`/`path`; `session = "command"` accepted on same; `session = "*"` rejected on `cmd`; `[[rule.run]] session = "session"` rejected; `stall_seconds` override parsed and validated `>= 1` |
| `capture`             | `open_session_phase_files` happy path; `open_session_fallback_stderr`; concurrent appends from reader + drain tasks (via real fds in tempdir) |

### 11.2 Integration tests

Each test reuses the existing `runtime::run_inner` harness with a `WORKFLOW.toml` whose phase bodies invoke a small bash script that fakes the AI by reading stdin and emitting stream-json on stdout.

- `session_two_turn`: pre and post are session-shape; iter 1 post directive `run`, iter 2 post directive `end`; verify a single child process across both turns (assert via the bash script writing its `$$` to a side-channel file once and observing the same pid on second invocation).
- `session_directive_advisory`: post turn emits `{"type":"thinking"}` then `{"directive":"end"}`; verify both lines in `post.events.jsonl` and `post.response.json` is the directive object.
- `session_after_terminal`: post turn emits the directive then a follow-up advisory line; verify advisory appears in `events.jsonl` but is not interpreted.
- `mixed_shape`: pre = session, run = command, post = session; verify pre + post share the same pid, run is a different pid per iter.
- `stall_command`: run phase sleeps past `[default.ai.command].stall_seconds`; binary exits 1; `run.exit_code` reflects SIGTERM (255 or signal-encoded); `run.stdout` is preserved.
- `stall_session`: pre phase is session; the fake AI never emits stdout; binary exits 1 with `Failure(Stall)`; capture preserved.
- `stall_override`: per-file `stall_seconds = 1` short-circuits a phase that would have survived under the default; assert the failure fires at the override window.
- `iter_exhausted_session`: `max_iterations = 2`; session post returns `run` twice; binary exits 1; supervisor shutdown observed via the bash script's exit-trap log line.
- `run_terminal`: run is a bash script that emits `{"type":"result","is_error":false,"result":"ok"}\n`; assert `run.terminal.json` exists and post template `{{ run.terminal.is_error }}` renders `false` (verified via post emitting it on stderr).
- `run_terminal_absent`: run is a plain shell command with no JSON; assert `run.terminal.json` does not exist; assert `{{ run.terminal.is_error }}` renders empty.

### 11.3 End-to-end smoke

`crates/roki-daemon/tests/e2e/session_smoke.rs` (new):

1. Spawn the `roki` binary with a `WORKFLOW.toml` whose pre and post are `prompt`-form (session by default). The fake AI is a bash script wired as `[default.ai.session].cli` that reads stdin in a loop and emits a JSON line per turn, alternating `directive: "run"` and `directive: "end"` based on a counter file.
2. POST one Linear-shaped webhook body.
3. Assert exit 0.
4. Assert layout: `<ticket-id>/cycle-<uuid>/iter-{1,2}/{pre,run,post}.{stdout,stderr}` plus `pre.events.jsonl`, `pre.response.json`, `post.events.jsonl`, `post.response.json`, `run.exit_code`. `run.terminal.json` is absent (run is plain shell).
5. Assert via the bash side-channel that pre and post observed the same pid across both iterations.

Existing `iteration_smoke.rs` (slice 1) is untouched: it uses `cmd`-form phases, which stay command-shape under slice 2.

## 12. Migration notes

### 12.1 Slice 1 call sites that change

- `engine::outcome::PhaseBody` variants gain `shape` and `stall_seconds` fields. All match arms in `phase.rs`, `cycle.rs`, `template.rs`, `context.rs`, `config/workflow.rs` are updated.
- `engine::outcome::FailureKind` gains `Stall` and `SessionSpawn`. Failure-table tests in `cycle.rs` are extended.
- `engine::cycle::run_cycle` switches on `phase.shape` per phase to dispatch between `CommandPhaseExecutor` and `SessionSupervisor`. The `PhaseExecutor` trait is retained for command-shape so slice-1 unit tests continue to pass.
- `engine::phase::CommandPhaseExecutor::execute` is rewritten: stdout is teed via a tokio task instead of redirected to a `File` directly, in order to (a) feed the watchdog and (b) extract `run.terminal.json`. The behaviour for non-stream-json stdout is unchanged on disk.
- `config::workflow::parse_phase_body` accepts `session` field on `prompt`/`path` and rejects on `cmd`. Slice 1's `WorkflowError::SessionShapeUnsupported` is removed; replaced by `WorkflowError::SessionRunUnsupported` for the run-only restriction.
- `config::roki` loader reads `[default.ai.session]` table. Default `stall_seconds = 600`.

### 12.2 capture.rs additions

`open_session_phase_files`, `open_session_fallback_stderr`, plus a `write_run_terminal_json(iter_dir, value)` helper. Slice 1 helpers untouched.

### 12.3 Backwards compatibility

Slice 1 `WORKFLOW.toml` files with `cmd`-only phases continue to load and run unchanged. A `prompt` phase that previously failed config load (because slice 1 rejected `[default.ai.session]` consumption) now loads successfully — operators must add `[default.ai.session].cli` to the runtime config. No feature flag is needed; the path is gated by the presence of session-shape phases at admission time.

### 12.4 Dependency additions

- `nix` for SIGTERM. `tokio::process::Child::kill()` already sends SIGKILL; the SIGTERM step uses `nix::sys::signal::kill(Pid::from_raw(child.id()? as i32), Signal::SIGTERM)`. Add `nix` with the `signal` feature; pin the version to whatever cargo resolves at slice-2 implementation time and record it in the implementation plan.
- No other new deps. `serde_json` (already present) handles streaming via `Deserializer::from_str` per line; the slice-1 `liquid`, `shell-words`, `async-trait` are reused.

## 13. Open questions deferred to slice 3+

- `[[on_failure]]` first-match cycle and the escalation queue.
- `[[cleanup]]` cycles.
- Worktree creation via `wt`. Once landed, session cwd resolves to the worktree (still fixed at spawn time per `fr:04 §46`).
- Persistent daemon lifecycle, queue preemption, cold-start enumeration.
- Hot reload of `WORKFLOW.toml` and `workflow/*.md`.
- Per-repo `WORKFLOW.toml` overrides.
- Run-phase session shape (currently rejected at load).
- `ROKI_RUN_TERMINAL_*` env export — Liquid-only suffices for now; revisit if operators report friction.

These are intentionally out of scope; the slice surface is sized so that operators can drive a single ticket end-to-end through one rule, one cycle, multiple iterations, with the long-lived AI session reused across pre/post turns and the run phase exposing claude/codex result events to the post template.
