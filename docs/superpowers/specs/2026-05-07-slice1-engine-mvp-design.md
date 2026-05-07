# Slice 1 — Engine MVP Design

Date: 2026-05-07
Scope: Layer the directive-driven iteration loop, iteration cap, Liquid template variables, and the canonical capture layout on top of the existing `roki-skeleton`. After this slice the daemon can run one ticket through a `pre → run → post` loop with multi-iteration, file-captured artefacts, and a hard iteration boundary.

## 1. Position in the Roadmap

Slice 1 collapses four roadmap specs that share a single coherent surface:

- `roki-engine-iteration-loop` — `fr:01` directive loop semantics.
- `roki-engine-iter-cap` — `[engine].max_iterations` enforcement.
- `roki-runtime-template-vars` — Liquid render of argv, stdin body, `ROKI_*` env.
- `roki-runtime-capture-layout` — `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{phase}.{stdout,stderr}` plus parsed-derivative files.

The skeleton (`roki-skeleton`, Wave 0) already provides: CLI start, webhook listener, assignee filter, hard-coded single-repo resolve, first-match `[[rule]]`, and command-form phase capture. It is preserved verbatim; this slice extends `runtime::run_inner` to call into a new `engine` module and replaces the flat `cycle-<uuid>/{stdout,stderr}` capture with the canonical layout.

Out-of-scope for slice 1, deferred to later slices:

- Session-shape phases and stream-json line-by-line parsing.
- Long-lived daemon and queue preemption (the binary still exits after one cycle, matching the skeleton).
- Stall detection (`[default.ai.*].stall_seconds`).
- `[[on_failure]]` cycles and the escalation queue.
- Worktree creation; phases run with `cwd = ghq base path` and operators treat the cwd as read-only.
- `[[cleanup]]` cycles.
- `run.terminal.json` (claude/codex stream-json `result` event).
- Cold-start ticket reconciliation.
- Per-repo `WORKFLOW.toml` overrides and hot reload.

## 2. Architecture

### 2.1 Module layout

A new `engine` submodule under `crates/roki-daemon/src/`:

```
crates/roki-daemon/src/
├── runtime.rs              // unchanged orchestrator: load, bind, drain, dispatch, exit
├── engine/
│   ├── mod.rs              // public re-exports for `runtime` and tests
│   ├── cycle.rs            // run_cycle: iteration loop + iter cap + phase dispatch
│   ├── phase.rs            // run_command_phase: spawn + capture + outcome translation
│   ├── directive.rs        // last-JSON-object scan + per-phase legal-set validation
│   ├── template.rs         // Liquid render of argv string and stdin body
│   ├── context.rs          // PhaseContext: Liquid object + ROKI_* env builder
│   └── outcome.rs          // PhaseOutcome / CycleOutcome / FailureKind enums
├── capture.rs              // rewritten: per-iter directory + per-phase file open
└── runner.rs               // deleted; behaviour absorbed by engine::phase
```

Call graph after the change:

```
runtime::run_inner
  └─ engine::cycle::run_cycle(ticket, matched_rule, &cfg, session_root)
      └─ for iter in 1..=cfg.engine.max_iterations:
           ├─ engine::phase::run_command_phase(Pre,  …)   // optional
           ├─ engine::phase::run_command_phase(Run,  …)
           └─ engine::phase::run_command_phase(Post, …)   // optional
              └─ engine::template::render(...)            // argv + stdin
              └─ engine::directive::parse(...)            // pre/post only
```

`engine::cycle::run_cycle` accepts a phase executor through a trait so unit tests can substitute deterministic fakes:

```rust
#[async_trait::async_trait]
pub trait PhaseExecutor {
    async fn execute(
        &self,
        kind: PhaseKind,
        body: &PhaseBody,
        ctx: &PhaseContext,
        iter_dir: &Path,
    ) -> Result<PhaseOutcome, PhaseInfraError>;
}
```

The production implementation in `engine::phase` constructs subprocesses; tests pass an in-memory fake.

### 2.2 Crate split

The slice keeps everything inside `crates/roki-daemon`. A separate `roki-engine` crate is intentionally not introduced — the engine surface is small enough that a submodule is cheaper to refactor later than to factor out now.

## 3. Cycle loop semantics

### 3.1 State transitions

The cycle loop mirrors `fr:01-engine-model §Phase loop`:

- iter 1: `pre` (if present) → `run` → `post`.
- post directive `pre` → next iter starts at `pre`.
- post directive `run` → next iter skips `pre`, starts at `run`.
- post directive `end` → cycle terminates.
- pre directive `end` → cycle terminates immediately (`run` and `post` skipped).
- pre absent → synthesised `directive: "run"` (treated identically to `skip_pre = true`).
- post absent → synthesised `directive: "end"` (cycle terminates after `run`).

Pseudo-code:

```text
ctx = PhaseContext::new(ticket, repo, cycle_id, &cfg)
skip_pre = false
for iter in 1..=cfg.engine.max_iterations:
    ctx.set_iter(iter)
    iter_dir = create_iter_dir(session_root, ticket_id, cycle_id, iter)

    if rule.pre.is_some() and not skip_pre:
        match exec.execute(Pre, rule.pre, &ctx, &iter_dir):
            Failure(kind)               -> return Failed { kind, iter }
            PreDirective(End,  payload) -> ctx.set_pre(payload); return Completed { iter }
            PreDirective(Run,  payload) -> ctx.set_pre(payload)
            // any other PhaseOutcome variant from Pre is a programmer error

    match exec.execute(Run, rule.run, &ctx, &iter_dir):
        Failure(kind)              -> return Failed { kind, iter }
        RunDone { exit_code, dur } -> ctx.set_run(exit_code, dur)

    next = if rule.post.is_some():
        match exec.execute(Post, rule.post, &ctx, &iter_dir):
            Failure(kind)                  -> return Failed { kind, iter }
            PostDirective(d, payload)      -> { ctx.set_post(payload); d }
    else:
        End

    skip_pre = (next == Run)
    match next:
        End                       -> return Completed { iter }
        Pre | Run                 -> continue   // iter cap re-checked at loop top

// fell off the loop = max_iterations reached after a non-end directive
return Failed { kind: IterExhausted, iter: max_iterations }
```

### 3.2 Iteration cap

`[engine].max_iterations` (default 10, min 1; canonical in `docs/reference/config.md`) is a hard boundary on **starting** a new iteration. When `post` returns `pre` or `run` and `iter == max_iterations`, the daemon does not spawn iteration `N+1`; it raises `FailureKind::IterExhausted` and aborts. Because slice 1 supports command-shape only, there is no session subprocess to drain, so no SIGTERM grace handling is needed.

Operators may preempt cooperatively from inside their pre/post body using `{{ cycle.iter }}` and `{{ config.max_iterations }}`. The engine does not synthesise any magic stdin signal.

### 3.3 Cycle outcome

```rust
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed    { kind: FailureKind, iter: u32 },
}
```

`runtime::run_inner` maps `Completed` to `ExitCode::SUCCESS` and `Failed` to `ExitCode::FAILURE`. Slice 1 has no `[[on_failure]]` routing, so a failed cycle terminates the binary directly with exit 1.

## 4. Phase execution

### 4.1 `run_command_phase` contract

```rust
pub enum PhaseKind { Pre, Run, Post }

pub enum PreDirective  { Run, End }
pub enum PostDirective { Pre, Run, End }

pub enum PhaseOutcome {
    PreDirective  { directive: PreDirective,  payload: serde_json::Value },
    PostDirective { directive: PostDirective, payload: serde_json::Value },
    RunDone       { exit_code: i32, duration_seconds: u64 },
    Failure       { kind: FailureKind },
}

pub enum FailureKind {
    Unparseable,    // pre/post: no JSON object on stdout, or last object lacks `directive`
    SchemaDrift,    // pre/post: directive value outside the legal set
    ProcessCrash,   // pre/post: non-zero exit AND no parseable terminal directive
    TemplateError,  // Liquid render of argv or stdin body failed
    IterExhausted,  // raised by `cycle::run_cycle`, not by `phase`
}

pub async fn run_command_phase(
    kind: PhaseKind,
    body: &PhaseBody,
    ctx: &PhaseContext,
    iter_dir: &Path,
) -> Result<PhaseOutcome, PhaseInfraError>;
```

`PhaseInfraError` covers spawn / wait / file-open errors that are infrastructure-level (not phase-level failures). They propagate up to `runtime::run_inner` and become exit 1 with a structured `tracing::error!` line.

### 4.2 Subprocess construction

Phase body forms (parsed from `WORKFLOW.toml` and resolved before render):

| Body form                              | argv source                                                   | stdin                                       |
| -------------------------------------- | ------------------------------------------------------------- | ------------------------------------------- |
| inline `cmd = "..."`                   | rendered cmd → `sh -c <rendered>`                             | empty (closed immediately)                  |
| inline `prompt = "..."`                | rendered `[default.ai.command].cli` (or frontmatter override) | rendered prompt string                      |
| `path = "workflow/foo.md"`             | rendered cli (frontmatter override or default command cli)    | rendered `.md` body (post-frontmatter)      |

Slice 1 supports all three. Frontmatter `session: "session"` is rejected at config-load time with a startup error (`ConfigError::SessionShapeUnsupported`); this slice ships the rejection, slice 2 ships the implementation.

Working directory is the **ghq base path** of the admission-resolved repo. Slice 1 does not create worktrees; phases run on the operator's main checkout and are expected to be read-only. The ghq base is resolved by spawning `ghq list -p <ghq>` once at cycle start and using stdout's first line; if the repo is missing, the cycle fails with `PhaseInfraError::RepoNotFound` (exit 1). Slice 2 introduces lazy worktree creation and the `wt` integration.

### 4.3 Capture and directive scan

```text
spawn:
  Command::new(argv[0])
    .args(&argv[1..])
    .env_clear()
    .envs(ROKI_* + PATH/HOME/USER passthrough)
    .current_dir(ghq_base)
    .stdin(piped if stdin_body else null)
    .stdout(File::create(iter_dir/<phase>.stdout))
    .stderr(File::create(iter_dir/<phase>.stderr))
    .spawn()

if stdin_body:
    child.stdin.write_all(rendered_body)
    drop(child.stdin)

let started = Instant::now()
let exit_status = child.wait().await
let duration_seconds = started.elapsed().as_secs()

post-exit (Run):
    write(iter_dir/run.exit_code, format!("{}\n", exit_status.code().unwrap_or(-1)))
    return RunDone { exit_code, duration_seconds }

post-exit (Pre/Post):
    bytes = read(iter_dir/<phase>.stdout)
    last_obj = scan_last_json_object(&bytes)
        None    -> if exit_status.success() { return Failure(Unparseable) }
                   else                     { return Failure(ProcessCrash) }
        Some(v) -> v
    write(iter_dir/<phase>.response.json, serde_json::to_string_pretty(&last_obj))
    let directive = last_obj["directive"].as_str()
        None    -> return Failure(Unparseable)
        Some(s) -> s
    parse `directive` against PhaseKind legal set:
        Pre  in {run, end}
        Post in {pre, run, end}
        else -> return Failure(SchemaDrift)
    return PreDirective | PostDirective + payload
```

`scan_last_json_object`: walks all top-level values in stdout via `serde_json::Deserializer::from_slice(...).into_iter::<Value>()`. Items that succeed are kept; items that fail to parse are dropped (the iterator may yield further `Ok` values after a failure if a later region of the stream is well-formed). The function returns the last `Ok(Value::Object)` from the sequence, or `None` if no top-level object parsed successfully. Bytes between objects (advisory text, ANSI, log lines) are not treated as input to JSON parsing — they sit between value boundaries and are ignored by `StreamDeserializer`. A non-object top-level value (string, number, array, null) is also ignored.

`run`-phase exit code is **not** translated into a `FailureKind`. A non-zero exit becomes part of `RunDone.exit_code` and is exposed to the next post phase as `{{ run.exit_code }}` and `ROKI_RUN_EXIT_CODE`. The operator's post template decides whether to retry, end, or fail.

### 4.4 Capture layout

```
<session_root>/
  <ticket-id>/                            // sanitiser: keep [A-Za-z0-9_-], replace others with '_'
    cycle-<uuid>/
      iter-1/
        pre.stdout    pre.stderr    pre.response.json     // when Pre runs
        run.stdout    run.stderr    run.exit_code         // always
        post.stdout   post.stderr   post.response.json    // when Post runs
      iter-2/
        ...
```

Files written by the engine itself:

- `<phase>.response.json` — `serde_json::to_string_pretty` of the parsed terminal object (Pre / Post only).
- `run.exit_code` — ASCII text `"<exit>\n"`.

Not written in slice 1:

- `<phase>.events.jsonl` — session-shape only.
- `run.terminal.json` — claude/codex stream-json `result` event detection lands in slice 2.

`capture.rs` API after rewrite:

```rust
pub fn create_iter_dir(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
    iter: u32,
) -> Result<PathBuf, CaptureError>;

pub fn open_phase_files(
    iter_dir: &Path,
    phase: PhaseKind,
) -> Result<(File, File), CaptureError>; // (stdout, stderr)

pub fn write_response_json(
    iter_dir: &Path,
    phase: PhaseKind,
    value: &serde_json::Value,
) -> Result<(), CaptureError>;

pub fn write_run_exit_code(
    iter_dir: &Path,
    exit_code: i32,
) -> Result<(), CaptureError>;
```

The old `CaptureLayout { dir, stdout, stderr }` struct is removed. `runner.rs` is deleted; its three unit tests migrate into `engine::phase`'s test module against the new file layout.

Ticket-id sanitiser: `[A-Za-z0-9_-]` survives unchanged; every other byte (including `/`, spaces, unicode) becomes `_`. Linear-style identifiers (`ENG-123`, `LOT-1234`) are kept verbatim. No length cap.

## 5. Templating and context

### 5.1 Engine

The `liquid` crate (Shopify-compatible Rust port) handles render. Templates are parsed once per phase invocation (no caching across phases in slice 1; the cost is negligible for personal workloads).

### 5.2 Render channels

| Channel | Source                                                                                                                                              | Render? |
| ------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ------- |
| argv    | `[default.ai.command].cli` (or workflow frontmatter `cli` override) for path / inline-prompt phases; the inline `cmd` string itself for inline-cmd | yes     |
| stdin   | `.md` body (post-frontmatter) for path phases; the inline `prompt` string for inline-prompt phases; nothing for inline-cmd phases                   | yes     |
| env     | `ROKI_*` scalar export of context fields, see §5.4                                                                                                  | n/a     |

Argv is split with `shell-words` after Liquid render (matches the skeleton's existing approach for `[default.ai.command].cli`). Inline `cmd` is wrapped in `sh -c <rendered>` so operators can keep using shell features.

### 5.3 Context fields

```rust
pub struct PhaseContext {
    ticket: TicketView,    // id, title, body, labels (Vec<String>), assignee, status
    repo:   RepoView,      // ghq
    cycle:  CycleView,     // id (Uuid), kind ("rule"), trigger ("runtime"), iter (u32)
    config: ConfigView,    // max_iterations
    pre:    Option<serde_json::Value>,  // most recent pre payload
    post:   Option<serde_json::Value>,  // most recent post payload
    run:    Option<RunView>,            // exit_code, duration_seconds
}
```

Mutations:

- `set_iter(iter)` — at the top of every iteration.
- `set_pre(payload)` — after a successful Pre.
- `set_run(exit_code, duration_seconds)` — after Run.
- `set_post(payload)` — after a successful Post.

The Liquid object is rebuilt from the context at the start of each phase via `serde_json::to_value` → `liquid::object::to_object`.

### 5.4 Env-var export

`fr:01-engine-model §Inter-phase data flow` table verbatim:

| Liquid variable                    | Env var                                     | Notes                                 |
| ---------------------------------- | ------------------------------------------- | ------------------------------------- |
| `{{ ticket.id }}`                  | `ROKI_TICKET_ID`                            |                                       |
| `{{ ticket.title }}`               | (Liquid only)                               | string can contain newlines           |
| `{{ ticket.body }}`                | (Liquid only)                               | string can contain newlines           |
| `{{ ticket.labels }}`              | (Liquid only)                               | array                                 |
| `{{ ticket.assignee }}`            | (Liquid only)                               | object                                |
| `{{ ticket.status }}`              | (Liquid only)                               |                                       |
| `{{ repo.ghq }}`                   | `ROKI_REPO`                                 |                                       |
| `{{ cycle.id }}`                   | `ROKI_CYCLE_ID`                             |                                       |
| `{{ cycle.kind }}`                 | `ROKI_CYCLE_KIND`                           | always `rule` in slice 1              |
| `{{ cycle.trigger }}`              | `ROKI_CYCLE_TRIGGER`                        | always `runtime` in slice 1           |
| `{{ cycle.iter }}`                 | `ROKI_CYCLE_ITER`                           |                                       |
| `{{ config.max_iterations }}`      | `ROKI_CONFIG_MAX_ITERATIONS`                |                                       |
| `{{ pre.<key> }}` (top-level)      | `ROKI_PRE_<KEY>` for top-level scalars only | `<KEY>` is field name uppercased      |
| `{{ post.<key> }}` (top-level)     | `ROKI_POST_<KEY>` for top-level scalars     | same naming rule as pre               |
| `{{ run.exit_code }}`              | `ROKI_RUN_EXIT_CODE`                        |                                       |
| `{{ run.duration_seconds }}`       | `ROKI_RUN_DURATION_SECONDS`                 |                                       |

Naming rule for `ROKI_PRE_*` / `ROKI_POST_*`: only top-level scalar fields (string / number / bool) are exported. Field name `<key>` is uppercased verbatim; non-`[A-Z0-9_]` characters cause the entry to be skipped with an `info!` log naming the offending key. Nested objects, arrays, and null fields are reachable through Liquid (`{{ pre.payload.foo }}`) but never through env.

`{{ run.terminal.* }}` is **not** exposed in slice 1.

PATH, HOME, and USER are passed through from the daemon's own environment so that operator phases can find binaries; the daemon does not propagate any other env var.

## 6. Error handling

### 6.1 Failure routing

Slice 1 has no `[[on_failure]]` cycle and no escalation queue. Every failure terminates the binary with exit 1.

| Trigger                                                                       | Surface                                       | Exit |
| ----------------------------------------------------------------------------- | --------------------------------------------- | ---- |
| Liquid render of argv or stdin body fails                                     | `PhaseOutcome::Failure(TemplateError)`        | 1    |
| Subprocess spawn fails (binary missing, fork error)                           | `PhaseInfraError::Spawn` → `SkeletonError`    | 1    |
| Pre/Post stdout has no JSON object                                            | `PhaseOutcome::Failure(Unparseable)`          | 1    |
| Pre/Post last JSON object lacks `directive` key                               | `PhaseOutcome::Failure(Unparseable)`          | 1    |
| Pre/Post `directive` value outside legal set                                  | `PhaseOutcome::Failure(SchemaDrift)`          | 1    |
| Pre/Post non-zero exit AND stdout has no parseable JSON object                | `PhaseOutcome::Failure(ProcessCrash)`         | 1    |
| Run exit non-zero                                                             | `RunDone { exit_code }` — operator decides    | n/a  |
| Post returns `pre`/`run` while `iter == max_iterations`                       | `CycleOutcome::Failed(IterExhausted)`         | 1    |
| `iter_dir` create fails / phase file open fails                               | `PhaseInfraError::Capture` → `SkeletonError`  | 1    |
| `ghq list -p` fails or returns no path                                        | `PhaseInfraError::RepoNotFound`               | 1    |

### 6.2 Forensics

On every failure the per-iter directory is left on disk so operators can inspect `pre.stdout`, `pre.stderr`, `post.stdout`, etc. The daemon does not delete `iter_dir` on failure. (Cleanup arrives with `[[cleanup]]` in a later slice.)

### 6.3 Structured events

Slice 1 emits the following tracing events through the existing pipeline (no new tier infrastructure; `fr:08` Tier 1 already exists from skeleton):

- `cycle_started`   — fields: `cycle.id`, `cycle.kind="rule"`, `cycle.trigger="runtime"`, `ticket.id`, `repo.ghq`.
- `phase_started`   — fields: `cycle.id`, `cycle.iter`, `phase` ∈ {`pre`,`run`,`post`}, argv head (truncated), cwd.
- `phase_completed` — fields: `cycle.id`, `cycle.iter`, `phase`, `exit_code`, `duration_seconds`, `directive` (when applicable), stderr head (truncated to 256 bytes) + stderr tail.
- `phase_failed`    — fields: `cycle.id`, `cycle.iter`, `phase`, `failure.kind`, stderr head + tail, capture path.
- `cycle_completed` — fields: `cycle.id`, `iters`.
- `cycle_aborted`   — fields: `cycle.id`, `iters`, `failure.kind`.

Secret redaction continues to rely on tracing-level redaction (skeleton already strips `[linear].token` and `[linear.webhook].secret` from any structured event field).

## 7. Testing strategy

### 7.1 Unit tests

| Module               | Coverage                                                                                                                                          |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `engine::directive`  | Pre × {`run`, `end`, missing `directive`, illegal value, no JSON, advisory text after terminal, multi-object stdout pick-last}; same for Post     |
| `engine::template`   | Render with `ticket.*` / `pre.*` / `cycle.iter` / missing var (Liquid empty default) / render error (bad filter) / shell-words split correctness  |
| `engine::context`    | env build: scalar / nested object skip / array skip / null skip / non-ascii key skip + info log / boolean / integer / float                       |
| `engine::cycle`      | All directive transitions via `PhaseExecutor` fake: `pre→end`, `pre→run→post→end`, `pre→run→post→pre`, `pre→run→post→run`, pre absent, post absent, iter cap with post=run, iter cap with post=pre, Failure short-circuits at each phase |
| `capture`            | `create_iter_dir` happy path, sanitiser (`/`, spaces, unicode), file open errors, `write_response_json`, `write_run_exit_code`                    |

### 7.2 Integration tests (in-process daemon)

Each test reuses the existing `runtime::run_inner` harness (tempdir + wiremock for `viewer { id }`) and replaces the rule body with a `sh -c` script that fakes an AI by emitting JSON on stdout.

- Two-iteration loop (`post = "run"` in iter 1, `post = "end"` in iter 2). Verify `iter-1/`, `iter-2/` exist with the expected `pre.response.json` / `run.exit_code` / `post.response.json` contents.
- `pre = "end"` short-circuits before `run` is spawned.
- Post absent → cycle terminates after Run (single iteration).
- Pre absent → first iter starts at Run.
- Iter cap collision: `max_iterations = 2`, post returns `"run"` twice; binary exits 1, last `iter_dir` contains the failed-cycle artefacts.
- `directive` parse failures: `Unparseable` (no JSON), `SchemaDrift` (`directive: "halt"`).
- Liquid var injection: `pre` body contains `printf "%s" "$ROKI_TICKET_ID" >&2`; assert `pre.stderr` matches the ticket id.
- `ROKI_PRE_*` skip rule: pre payload `{"directive":"run","my-field":"x"}` → `info!` log naming `my-field`, env not set (verified via a run cmd that re-emits its own env).

### 7.3 End-to-end smoke

`crates/roki-daemon/tests/e2e/iteration_smoke.rs` — new file, mirrors the existing `skeleton_smoke.rs` style:

1. Spawn the `roki` binary with a `WORKFLOW.toml` whose rule defines all three phases as `cmd` form. The fake AI is a bash script that consults a temp counter file to decide whether to emit `directive: "run"` or `directive: "end"`.
2. POST one Linear-shaped webhook body.
3. Assert the binary exits 0.
4. Assert the on-disk layout matches `<session_root>/<ticket-id>/cycle-<uuid>/iter-{1,2}/{pre,run,post}.{stdout,stderr}` plus `pre.response.json`, `run.exit_code`, `post.response.json`.

The existing `skeleton_smoke.rs` is updated minimally: its assertion paths change from `cycle-<uuid>/{stdout,stderr}` to `<ticket-id>/cycle-<uuid>/iter-1/run.{stdout,stderr}`. The rest of the contract (single-cycle exit-zero, second POST → 503) is preserved.

## 8. Migration notes

### 8.1 Skeleton call sites that change

- `runtime::run_inner` lines 154–171 (cycle execution + flush) → replaced by `let outcome = engine::cycle::run_cycle(...).await; map outcome to ExitCode`.
- `runner.rs` deleted.
- `capture.rs` rewritten as described in §4.4. Old `CaptureLayout` consumers (only `runtime::run_inner`) updated.
- `skeleton_smoke.rs` assertion paths updated.

### 8.2 Backwards compatibility

The `roki.toml` schema does not change. `WORKFLOW.toml` accepts `[[rule.pre]]`, `[[rule.post]]` blocks that the skeleton previously ignored; the schema parser already permits unknown blocks (rule body is the only place that matters). No feature flag is needed.

### 8.3 Dependency additions

The skeleton's `crates/roki-daemon/Cargo.toml` does not yet pull in any of these; all three are added as direct deps in this slice:

- `liquid` — Liquid template engine.
- `shell-words` — argv split after Liquid render of the cli line.
- `async-trait` — required for `PhaseExecutor` so `engine::cycle::run_cycle` can accept a fake executor in unit tests.

## 9. Open questions deferred to slice 2+

- Stream-json line-by-line parsing for session-shape phases.
- `run.terminal.json` extraction.
- Stall window enforcement.
- `[[on_failure]]` first-match cycle.
- Persistent daemon lifecycle, queue preemption, cold-start enumeration.
- Worktree creation via `wt`.
- Hot reload of `WORKFLOW.toml` and `workflow/*.md`.
- Per-repo `WORKFLOW.toml` overrides.

These are intentionally out of scope; the slice surface is sized so that operators can drive a single ticket end-to-end through one rule, one cycle, multiple iterations, with full Liquid templating and on-disk forensics.
