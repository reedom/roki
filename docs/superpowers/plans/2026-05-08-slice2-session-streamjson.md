# Slice 2 Session-Shape & Stream-JSON Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Layer the long-lived session subprocess, stream-json line-by-line parsing, stall detection (SIGTERM + grace + SIGKILL), per-file `stall_seconds` override, and `run.terminal.json` extraction on top of slice 1's command-shape engine. After this plan, pre/post phases can reuse a single AI subprocess across iterations, the run phase exposes claude/codex stream-json `result` events to the post template via `{{ run.terminal.* }}`, and any subprocess that goes silent past its stall window is terminated.

**Architecture:** A new `SessionSupervisor` struct (one per cycle) owns the long-lived child plus a tokio reader task. `engine::cycle::run_cycle` dispatches per phase: command-shape goes through the existing `CommandPhaseExecutor`, session-shape goes through the supervisor's `run_turn`. Both shapes share a new `Watchdog` (idle-stdout-byte detector) and a new `stream` line-splitter that recognises the claude/codex `result` event for the run phase. Per-file `session` and `stall_seconds` overrides come from `workflow/*.md` frontmatter, parsed once at config load.

**Tech Stack:** Rust 2024 (workspace edition), `tokio` async runtime, `nix` (`signal` feature) for SIGTERM, `serde_yaml_ng` for `workflow/*.md` frontmatter (avoids the deprecated `serde_yaml`), plus the slice-1 deps (`liquid`, `shell-words`, `async-trait`, `serde_json`, `tempfile`, `wiremock`, `reqwest`).

**Spec:** `docs/superpowers/specs/2026-05-08-slice2-session-streamjson-design.md` (commits `b5b...` and the drift-fix follow-up on branch `slice2-session-streamjson`).

**Working branch:** `slice2-session-streamjson` (already created and contains the spec commits).

---

## File Structure

### Created

| Path                                                           | Responsibility                                                                                          |
| -------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `crates/roki-daemon/src/engine/session.rs`                     | `SessionSupervisor` (long-lived child + reader task + per-turn directive flow + iter_exhausted shutdown). |
| `crates/roki-daemon/src/engine/stream.rs`                      | `LineSplitter`, `scan_directive_line`, `scan_run_terminal_line` (recognise claude/codex `result` event). |
| `crates/roki-daemon/src/engine/stall.rs`                       | `Watchdog` struct: idle-stdout watchdog + SIGTERM/grace/SIGKILL helper.                                 |
| `crates/roki-daemon/src/config/workflow_md.rs`                 | `parse_workflow_md_frontmatter` — pulls `session`, `stall_seconds`, `cli` from YAML frontmatter.         |
| `crates/roki-daemon/tests/e2e/session_smoke.rs`                | End-to-end smoke for a 2-iter session-shape cycle.                                                      |
| `crates/roki-daemon/tests/e2e/stall_smoke.rs`                  | End-to-end smoke for stall + SIGTERM termination on a hung run phase.                                   |
| `crates/roki-daemon/tests/e2e/run_terminal_smoke.rs`           | End-to-end smoke for `run.terminal.json` extraction and `{{ run.terminal.* }}` post substitution.       |
| `crates/roki-daemon/tests/e2e/fixtures/fake_session_agent.sh`  | Bash fake AI consumed by `session_smoke` (reads stdin, emits stream-json, alternates `run`/`end`).      |
| `crates/roki-daemon/tests/e2e/fixtures/sleep_run.sh`           | Bash script for `stall_smoke` that sleeps past the configured stall window.                             |
| `crates/roki-daemon/tests/e2e/fixtures/fake_run_terminal.sh`   | Bash script that emits a single `{"type":"result", ...}` line for `run_terminal_smoke`.                 |

### Modified

| Path                                                | Change                                                                                                              |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `crates/roki-daemon/Cargo.toml`                     | Add `nix` (signal feature), `serde_yaml`. Add `[[test]]` entries for the three new e2e files.                       |
| `crates/roki-daemon/src/engine/mod.rs`              | Declare new `session`, `stream`, `stall` submodules. Re-export `SessionSupervisor`, `PhaseShape`.                   |
| `crates/roki-daemon/src/engine/outcome.rs`          | Add `PhaseShape`. Extend `PhaseBody::Path` with `shape`/`stall_seconds`. Extend `FailureKind` with `Stall`/`SessionSpawn`. |
| `crates/roki-daemon/src/engine/phase.rs`            | `CommandPhaseExecutor::execute` → tee stdout via tokio task, integrate `Watchdog`, write `run.terminal.json` mid-stream. |
| `crates/roki-daemon/src/engine/context.rs`          | `RunView` gains `terminal: Option<serde_json::Value>`. `set_run` accepts the terminal value.                        |
| `crates/roki-daemon/src/engine/cycle.rs`            | Resolve per-phase shape. Construct `SessionSupervisor` lazily. Dispatch session phases through supervisor.          |
| `crates/roki-daemon/src/capture.rs`                 | Add `open_session_phase_files` + `write_run_terminal_json`.                                                         |
| `crates/roki-daemon/src/config/mod.rs`              | Declare new `workflow_md` submodule.                                                                                |
| `crates/roki-daemon/src/config/workflow.rs`         | Drop slice-1 session rejection. For `path` form: parse `.md` frontmatter via `workflow_md`. Validate run resolves to `Command`. Reject `session` field on inline forms. |
| `crates/roki-daemon/src/config/roki.rs`             | Promote `DefaultAiSessionSection` to applied-with-defaults shape (cli: Option<String>, stall_seconds: u32 default 600). Add `stall_seconds` to `DefaultAiCommandSection` (default 300). |
| `crates/roki-daemon/src/error.rs`                   | Extend `WorkflowError` (`SessionRunUnsupported`, `InvalidSessionField`, `InvalidStallSeconds`, `WorkflowMdFrontmatter`). Extend `PhaseInfraError` (`SessionSpawn`, `SessionStdinClosed`, `SessionStdoutClosed`). |

### Deleted

(none)

---

## Cross-Task Conventions

### Test commands

- Whole crate: `cargo test -p roki-daemon --features test-support`
- Single unit: `cargo test -p roki-daemon --features test-support --lib <module>::tests::<name> -- --nocapture`
- Single integration: `cargo test -p roki-daemon --features test-support --test <name>`

The smoke tests already require `--features test-support` (the slice-1 `ROKI_LINEAR_GRAPHQL_URL` seam). New tests follow the same convention.

### Commit style

- Conventional Commits.
- One commit per task end (after the task's final test passes).
- Title ≤ 50 chars, lowercase verb, scoped to `engine`, `capture`, `runtime`, `config`, `tests`, or `deps`.
- Body explains the why when not obvious from the title.
- No emojis.
- Co-authored trailer included on every commit:

  ```
  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
  ```

### Task dependency order

- **Foundation (Tasks 1-5)**: deps, error variants, config schema (roki + workflow.md frontmatter), `PhaseShape` + `PhaseBody::Path` extension. Nothing wires into the cycle yet.
- **Primitives (Tasks 6-9)**: `engine::stream`, `engine::stall::Watchdog`, capture helpers (`open_session_phase_files`, `write_run_terminal_json`). Each is unit-tested in isolation.
- **Command-shape upgrades (Tasks 10-11)**: rewrite `CommandPhaseExecutor::execute` to tee stdout, integrate watchdog, extract `run.terminal.json`.
- **Session supervisor (Tasks 12-15)**: `SessionSupervisor::spawn`, `run_turn`, `shutdown`, between-turn stderr buffer.
- **Integration (Tasks 16-18)**: `PhaseContext.run.terminal`, `cycle::run_cycle` shape dispatch, drop slice-1 rejection.
- **Smoke tests (Tasks 19-21)**: session_smoke, stall_smoke, run_terminal_smoke.

---

## Task 1: Add `nix` and `serde_yaml` dependencies

**Files:**
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add the new `[dependencies]` entries**

Edit `crates/roki-daemon/Cargo.toml`. Insert these lines into the alphabetical position inside `[dependencies]`:

```toml
nix = { version = "0.29", features = ["signal"] }
serde_yaml = "0.9"
```

The `[dependencies]` block (after edit) should read (showing the alphabetical neighbours):

```toml
[dependencies]
anyhow = "1"
async-trait = "0.1"
axum = "0.7"
clap = { version = "4", features = ["derive"] }
liquid = "0.26"
nix = { version = "0.29", features = ["signal"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
shell-words = "1"
thiserror = "2"
tokio = { version = "1", features = ["full"] }
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["fmt"] }
uuid = { version = "1", features = ["v4"] }
```

- [ ] **Step 2: Verify the workspace builds**

Run: `cargo build -p roki-daemon`
Expected: build succeeds. Cargo fetches the new crates.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-daemon/Cargo.toml Cargo.lock
git commit -m "deps: add nix and serde_yaml" -m "nix supplies SIGTERM (tokio::process::Child::kill is SIGKILL-only). serde_yaml parses workflow/*.md frontmatter for slice-2 per-file shape and stall_seconds overrides." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Extend `DefaultAiSessionSection` and `DefaultAiCommandSection` with `stall_seconds`

**Files:**
- Modify: `crates/roki-daemon/src/config/roki.rs`

Slice 1's `DefaultAiCommandSection` carries only `cli: String`; `DefaultAiSessionSection` carries only `cli: Option<String>` and is "accepted-without-applying". Slice 2 needs `stall_seconds` on both, with defaults `300` (command) and `600` (session) per `docs/reference/config.md`.

- [ ] **Step 1: Write the failing test for `stall_seconds` defaults**

Append the following test to the `mod tests` block in `crates/roki-daemon/src/config/roki.rs`:

```rust
    #[test]
    fn stall_seconds_defaults_when_absent() {
        let toml = r#"
[linear]
token = "t"

[linear.webhook]
bind = "127.0.0.1"
port = 7000

[default.ai.command]
cli = "claude --print"

[engine]

[paths]
workflow = "./WORKFLOW.toml"
session_root = "./.roki/sessions"
"#;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = RokiConfig::load(&path).unwrap();
        assert_eq!(cfg.default_ai_command.stall_seconds, 300);
        assert_eq!(
            cfg.default_ai_session
                .as_ref()
                .map(|s| s.stall_seconds)
                .unwrap_or(600),
            600
        );
    }

    #[test]
    fn stall_seconds_zero_is_rejected() {
        let toml = r#"
[linear]
token = "t"

[linear.webhook]
bind = "127.0.0.1"
port = 7000

[default.ai.command]
cli = "claude --print"
stall_seconds = 0

[engine]

[paths]
workflow = "./WORKFLOW.toml"
session_root = "./.roki/sessions"
"#;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml).unwrap();
        let err = RokiConfig::load(&path).unwrap_err();
        match err {
            RokiConfigError::TypeMismatch { key, .. } => {
                assert_eq!(key, "default.ai.command.stall_seconds");
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the new tests and confirm they fail to compile**

Run: `cargo test -p roki-daemon --features test-support --lib config::roki::tests::stall_seconds_defaults_when_absent`
Expected: compile error — `stall_seconds` field missing on `DefaultAiCommandSection` and `DefaultAiSessionSection`.

- [ ] **Step 3: Add `stall_seconds` fields and validation**

Edit `crates/roki-daemon/src/config/roki.rs`. Replace the existing `DefaultAiCommandSection` and `DefaultAiSessionSection` definitions:

```rust
/// `[default.ai.command]` section.
#[derive(Clone, Debug)]
pub struct DefaultAiCommandSection {
    pub cli: String,
    pub stall_seconds: u32,
}

/// `[default.ai.session]` section.
///
/// Slice 2 promotes the section from "accepted-without-applying" to a
/// loaded shape: `cli` stays optional (only required when an actual phase
/// resolves to session shape, checked at cycle start), `stall_seconds` gets
/// the canonical default 600.
#[derive(Clone, Debug)]
pub struct DefaultAiSessionSection {
    pub cli: Option<String>,
    pub stall_seconds: u32,
}
```

Then, in the same file, locate the `RawDefault*` shapes used during validation. Replace the existing raw shapes for command/session:

```rust
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawDefaultAiCommand {
    cli: Option<String>,
    stall_seconds: Option<i64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawDefaultAiSession {
    cli: Option<String>,
    stall_seconds: Option<i64>,
}
```

(If the existing struct names differ — e.g. they include the `Section` suffix — match the existing names; the only change is the new `stall_seconds: Option<i64>` field.)

Then in the `validate` impl on `RawRokiConfig`, replace the existing `default_ai_command` and `default_ai_session` materialisation blocks with:

```rust
        // [default.ai.command]
        let raw_default_command = raw_default_ai.command.unwrap_or_default();
        let cli = required_string(
            raw_default_command.cli,
            "default.ai.command.cli",
            path,
        )?;
        let cmd_stall = match raw_default_command.stall_seconds {
            None => 300u32,
            Some(n) if n >= 1 => n as u32,
            Some(_) => {
                return Err(RokiConfigError::TypeMismatch {
                    path: path.to_path_buf(),
                    key: "default.ai.command.stall_seconds".to_string(),
                    expected: "integer >= 1".to_string(),
                });
            }
        };
        let default_ai_command = DefaultAiCommandSection {
            cli,
            stall_seconds: cmd_stall,
        };

        // [default.ai.session]
        let default_ai_session = match raw_default_ai.session {
            None => None,
            Some(raw_session) => {
                let session_stall = match raw_session.stall_seconds {
                    None => 600u32,
                    Some(n) if n >= 1 => n as u32,
                    Some(_) => {
                        return Err(RokiConfigError::TypeMismatch {
                            path: path.to_path_buf(),
                            key: "default.ai.session.stall_seconds".to_string(),
                            expected: "integer >= 1".to_string(),
                        });
                    }
                };
                Some(DefaultAiSessionSection {
                    cli: raw_session.cli,
                    stall_seconds: session_stall,
                })
            }
        };
```

(`required_string` is the existing slice-1 helper for converting `Option<String>` into a `RokiConfigError::MissingField` on `None`. If your local helper has a different name, use whatever slice 1 used for `default.ai.command.cli` — visible in the surrounding code.)

- [ ] **Step 4: Update slice-1 callers**

The slice-1 codebase reads `cfg.default_ai_command.cli` (string). No change needed there. If the existing slice-1 test `accepted_without_applying_default_ai_session_loads_ok` reads `cli: Option<String>`, it now also has `stall_seconds: u32` available — leave it unchanged unless the assertion now fails to compile, in which case extend it to also assert `stall_seconds == 600`.

- [ ] **Step 5: Run all roki config tests**

Run: `cargo test -p roki-daemon --features test-support --lib config::roki`
Expected: every test passes.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/config/roki.rs
git commit -m "config: add stall_seconds to ai command/session" -m "Slice 2 needs the per-shape stall window. Defaults match docs/reference/config.md (command 300, session 600); both reject zero/negative." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Extend `WorkflowError` with slice-2 variants

**Files:**
- Modify: `crates/roki-daemon/src/error.rs`

- [ ] **Step 1: Add new `WorkflowError` variants**

In `crates/roki-daemon/src/error.rs`, replace the existing `WorkflowError` enum with:

```rust
/// Errors raised while loading `WORKFLOW.toml` or `workflow/*.md` frontmatter.
#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("WORKFLOW.toml not found: {path}")]
    MissingFile { path: PathBuf },

    #[error("WORKFLOW.toml unreadable at {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("WORKFLOW.toml parse error at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("WORKFLOW.toml at {path} missing required key '{key}'")]
    MissingField { path: PathBuf, key: String },

    #[error("invalid workflow.toml at {path}: unsupported when.* key '{key}'")]
    UnsupportedWhen { path: PathBuf, key: String },

    #[error("invalid workflow.toml at {path}: unsupported run.* form '{key}'")]
    UnsupportedRunForm { path: PathBuf, key: String },

    #[error(
        "invalid workflow.toml at {path}: run phase resolved to session shape \
         (slice-2 unsupported; lift via path-form .md frontmatter `session: \"command\"`)"
    )]
    SessionRunUnsupported { path: PathBuf },

    #[error(
        "invalid workflow .md frontmatter at {path}: \
         field 'session' has unsupported value '{value}' (allowed: \"session\", \"command\")"
    )]
    InvalidSessionField { path: PathBuf, value: String },

    #[error(
        "invalid workflow .md frontmatter at {path}: \
         field 'stall_seconds' must be an integer >= 1, got '{value}'"
    )]
    InvalidStallSeconds { path: PathBuf, value: String },

    #[error("workflow .md frontmatter parse error at {path}: {reason}")]
    WorkflowMdFrontmatter { path: PathBuf, reason: String },
}
```

- [ ] **Step 2: Add Display tests for each new variant**

Append to the `mod tests` block in `crates/roki-daemon/src/error.rs`:

```rust
    #[test]
    fn workflow_session_run_unsupported_display() {
        let e = WorkflowError::SessionRunUnsupported {
            path: PathBuf::from("/tmp/W.toml"),
        };
        assert!(format!("{e}").contains("/tmp/W.toml"));
        assert!(format!("{e}").contains("session shape"));
    }

    #[test]
    fn workflow_invalid_session_field_display() {
        let e = WorkflowError::InvalidSessionField {
            path: PathBuf::from("/tmp/foo.md"),
            value: "yolo".to_string(),
        };
        let s = format!("{e}");
        assert!(s.contains("/tmp/foo.md"));
        assert!(s.contains("yolo"));
    }

    #[test]
    fn workflow_invalid_stall_seconds_display() {
        let e = WorkflowError::InvalidStallSeconds {
            path: PathBuf::from("/tmp/foo.md"),
            value: "0".to_string(),
        };
        assert!(format!("{e}").contains("stall_seconds"));
        assert!(format!("{e}").contains("0"));
    }

    #[test]
    fn workflow_md_frontmatter_display() {
        let e = WorkflowError::WorkflowMdFrontmatter {
            path: PathBuf::from("/tmp/foo.md"),
            reason: "missing closing '---'".to_string(),
        };
        let s = format!("{e}");
        assert!(s.contains("/tmp/foo.md"));
        assert!(s.contains("missing closing"));
    }
```

- [ ] **Step 3: Run the error tests**

Run: `cargo test -p roki-daemon --features test-support --lib error::tests`
Expected: every test passes.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/error.rs
git commit -m "error: add slice-2 workflow error variants" -m "Slice 2 introduces .md frontmatter parsing and per-phase shape resolution. SessionRunUnsupported, InvalidSessionField, InvalidStallSeconds, and WorkflowMdFrontmatter cover the new failure surfaces." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Extend `PhaseInfraError` with session variants and add `Stall` to `FailureKind`

**Files:**
- Modify: `crates/roki-daemon/src/error.rs`
- Modify: `crates/roki-daemon/src/engine/outcome.rs`

- [ ] **Step 1: Write the failing test for the new `FailureKind` variants**

Append to `mod tests` in `crates/roki-daemon/src/engine/outcome.rs`:

```rust
    #[test]
    fn failure_kind_stall_str_round_trip() {
        assert_eq!(FailureKind::Stall.as_str(), "stall");
        assert_eq!(FailureKind::SessionSpawn.as_str(), "session_spawn");
    }
```

- [ ] **Step 2: Run the test and confirm it fails**

Run: `cargo test -p roki-daemon --features test-support --lib engine::outcome::tests::failure_kind_stall_str_round_trip`
Expected: compile error — `FailureKind::Stall` and `FailureKind::SessionSpawn` are missing.

- [ ] **Step 3: Add the new variants to `FailureKind`**

In `crates/roki-daemon/src/engine/outcome.rs`, locate the `FailureKind` enum and `as_str` impl. Replace both:

```rust
/// Directive-level failure kinds. Distinct from `PhaseInfraError`, which
/// represents infrastructure-level failures that escape the cycle as a
/// `Result::Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Pre/Post: stdout has no JSON object, or the last JSON object lacks
    /// `directive`.
    Unparseable,
    /// Pre/Post: `directive` value outside the legal set for the phase.
    SchemaDrift,
    /// Pre/Post: non-zero exit and stdout has no parseable JSON object.
    ProcessCrash,
    /// Liquid render of argv or stdin body failed before launch.
    TemplateError,
    /// Post returned `pre` or `run` while `iter == max_iterations`.
    IterExhausted,
    /// Stdout silent for `stall_seconds`; supervisor SIGTERMed (and SIGKILLed
    /// after grace if necessary). Applies to both shapes.
    Stall,
    /// SessionSupervisor failed to spawn the long-lived child (missing cli,
    /// exec error). Reported as a phase failure on the first session-shape
    /// phase the cycle attempts.
    SessionSpawn,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::Unparseable => "unparseable",
            FailureKind::SchemaDrift => "schema_drift",
            FailureKind::ProcessCrash => "process_crash",
            FailureKind::TemplateError => "template_error",
            FailureKind::IterExhausted => "iter_exhausted",
            FailureKind::Stall => "stall",
            FailureKind::SessionSpawn => "session_spawn",
        }
    }
}
```

- [ ] **Step 4: Add the new `PhaseInfraError` variants**

In `crates/roki-daemon/src/error.rs`, locate the `PhaseInfraError` enum. Replace it with:

```rust
/// Errors raised by the engine's phase executor that are infrastructure-level
/// rather than directive-level failures.
#[derive(Debug, Error)]
pub enum PhaseInfraError {
    #[error("phase failed to spawn '{cmd}': {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("phase failed to wait on '{cmd}': {source}")]
    Wait {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("phase failed to read workflow body at {path}: {source}")]
    WorkflowBodyUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("phase '{cmd}' has no stdin handle but a rendered stdin body was prepared")]
    StdinUnavailable { cmd: String },

    #[error("phase failed to write stdin for '{cmd}': {source}")]
    StdinWrite {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("ghq base path not found for '{ghq}'")]
    RepoNotFound { ghq: String },

    #[error("cycle failed: {} at iter {iter}", kind.as_str())]
    CycleFailed {
        kind: FailureKind,
        iter: u32,
    },

    #[error("phase executor returned unexpected outcome variant '{got_variant}' for phase {} at iter {iter}", phase.as_str())]
    ExecutorContract {
        phase: PhaseKind,
        got_variant: &'static str,
        iter: u32,
    },

    #[error("session subprocess failed to spawn '{cli}': {source}")]
    SessionSpawn {
        cli: String,
        #[source]
        source: std::io::Error,
    },

    #[error("session subprocess stdin closed unexpectedly during turn for phase {}", phase.as_str())]
    SessionStdinClosed { phase: PhaseKind },

    #[error("session subprocess stdout closed before any directive on phase {}", phase.as_str())]
    SessionStdoutClosed { phase: PhaseKind },

    #[error("[default.ai.session].cli not configured but cycle requires session shape")]
    SessionCliMissing,

    #[error(transparent)]
    Capture(#[from] CaptureError),
}
```

- [ ] **Step 5: Run the outcome tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::outcome`
Expected: all pass.

- [ ] **Step 6: Run the error tests**

Run: `cargo test -p roki-daemon --features test-support --lib error`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/src/engine/outcome.rs crates/roki-daemon/src/error.rs
git commit -m "engine: add Stall, SessionSpawn failure kinds" -m "Slice 2 introduces stall detection and session subprocess spawn. PhaseInfraError gains SessionSpawn / SessionStdinClosed / SessionStdoutClosed / SessionCliMissing for infra-level errors; FailureKind gains Stall and SessionSpawn for directive-routed failures." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Add `PhaseShape`, extend `PhaseBody::Path`, and parse `workflow/*.md` frontmatter

**Files:**
- Create: `crates/roki-daemon/src/config/workflow_md.rs`
- Modify: `crates/roki-daemon/src/config/mod.rs`
- Modify: `crates/roki-daemon/src/engine/outcome.rs`

- [ ] **Step 1: Add `PhaseShape` and extend `PhaseBody::Path`**

Edit `crates/roki-daemon/src/engine/outcome.rs`. Add the new `PhaseShape` enum next to `PhaseKind`:

```rust
/// Subprocess wire shape per phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseShape {
    /// Long-lived AI subprocess reused across all pre/post turns of the cycle.
    Session,
    /// One-shot subprocess per phase invocation.
    Command,
}

impl PhaseShape {
    pub fn as_str(self) -> &'static str {
        match self {
            PhaseShape::Session => "session",
            PhaseShape::Command => "command",
        }
    }
}
```

Replace the existing `PhaseBody::Path` variant with:

```rust
    /// `path = "workflow/<file>.md"`. Resolved at config-load time against
    /// the workflow file's parent directory.
    Path {
        path: PathBuf,
        cli_override: Option<String>,
        /// Resolved from the .md frontmatter `session:` field.
        /// Defaults to `Session` when the field is absent.
        shape: PhaseShape,
        /// Resolved from the .md frontmatter `stall_seconds:` field.
        /// `None` means "fall back to the shape default in `[default.ai.*].stall_seconds`".
        stall_seconds: Option<u32>,
    },
```

`InlineCmd` and `InlinePrompt` are unchanged; their shape is fixed by variant identity (`InlineCmd` → Command, `InlinePrompt` → Session). Their stall window comes from `[default.ai.*].stall_seconds` at execute time.

- [ ] **Step 2: Add a helper that returns each `PhaseBody`'s shape**

Append to the `impl PhaseBody` block (or create one if absent) in `crates/roki-daemon/src/engine/outcome.rs`:

```rust
impl PhaseBody {
    /// Wire shape this phase body resolves to. Variant-fixed for inline
    /// forms; field-driven for the `Path` form (already resolved at config
    /// load via `workflow_md::parse_workflow_md_frontmatter`).
    pub fn shape(&self) -> PhaseShape {
        match self {
            PhaseBody::InlineCmd { .. } => PhaseShape::Command,
            PhaseBody::InlinePrompt { .. } => PhaseShape::Session,
            PhaseBody::Path { shape, .. } => *shape,
        }
    }

    /// Per-file `stall_seconds` override, or `None` for shape-default.
    /// Inline forms never carry an override.
    pub fn stall_seconds_override(&self) -> Option<u32> {
        match self {
            PhaseBody::InlineCmd { .. } | PhaseBody::InlinePrompt { .. } => None,
            PhaseBody::Path { stall_seconds, .. } => *stall_seconds,
        }
    }
}
```

- [ ] **Step 3: Write the workflow_md parser test fixtures**

Create `crates/roki-daemon/src/config/workflow_md.rs` with the following content:

```rust
//! `workflow/*.md` frontmatter parser.
//!
//! Slice 2 reads three optional YAML fields from the leading `---/---`
//! frontmatter block of a workflow .md file:
//! - `session: "session" | "command"` — sets `PhaseBody::Path::shape`.
//! - `stall_seconds: <int>` — sets `PhaseBody::Path::stall_seconds`.
//! - `cli: "<liquid template>"` — already honored by slice 1 as a CLI
//!   override; slice 2 keeps the same field but reads it via this parser
//!   instead of the ad-hoc scan.
//!
//! Missing frontmatter is **not** an error: the file is treated as
//! `session: "session"` (default) with no `stall_seconds` override and no
//! `cli` override. The full contract is in `fr:04 §29-34, §114-122` and in
//! `docs/superpowers/specs/2026-05-08-slice2-session-streamjson-design.md
//! §3.4`.

use std::path::Path;

use serde::Deserialize;

use crate::engine::outcome::PhaseShape;
use crate::error::WorkflowError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowMdHeader {
    pub shape: PhaseShape,
    pub stall_seconds: Option<u32>,
    pub cli: Option<String>,
}

impl Default for WorkflowMdHeader {
    fn default() -> Self {
        Self {
            shape: PhaseShape::Session,
            stall_seconds: None,
            cli: None,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawHeader {
    session: Option<String>,
    stall_seconds: Option<i64>,
    cli: Option<String>,
}

/// Parse the leading `---/---` YAML frontmatter from `body` and return both
/// the header struct and the post-frontmatter body slice. When `body` does
/// not begin with `---\n` (or `---\r\n`), returns the default header and the
/// full body.
pub fn parse_workflow_md_frontmatter<'a>(
    path: &Path,
    body: &'a str,
) -> Result<(WorkflowMdHeader, &'a str), WorkflowError> {
    let Some(rest) = body.strip_prefix("---\n").or_else(|| body.strip_prefix("---\r\n")) else {
        return Ok((WorkflowMdHeader::default(), body));
    };

    let Some(end) = find_closing_delimiter(rest) else {
        return Err(WorkflowError::WorkflowMdFrontmatter {
            path: path.to_path_buf(),
            reason: "missing closing '---' delimiter".to_string(),
        });
    };

    let yaml = &rest[..end.start];
    let raw: RawHeader = serde_yaml_ng::from_str(yaml).map_err(|err| {
        WorkflowError::WorkflowMdFrontmatter {
            path: path.to_path_buf(),
            reason: format!("yaml parse error: {err}"),
        }
    })?;

    let shape = match raw.session.as_deref() {
        None | Some("session") => PhaseShape::Session,
        Some("command") => PhaseShape::Command,
        Some(other) => {
            return Err(WorkflowError::InvalidSessionField {
                path: path.to_path_buf(),
                value: other.to_string(),
            });
        }
    };

    let stall_seconds = match raw.stall_seconds {
        None => None,
        Some(n) if n >= 1 => Some(n as u32),
        Some(other) => {
            return Err(WorkflowError::InvalidStallSeconds {
                path: path.to_path_buf(),
                value: other.to_string(),
            });
        }
    };

    let header = WorkflowMdHeader {
        shape,
        stall_seconds,
        cli: raw.cli.filter(|s| !s.is_empty()),
    };

    Ok((header, &rest[end.end..]))
}

struct Span {
    start: usize,
    end: usize,
}

fn find_closing_delimiter(rest: &str) -> Option<Span> {
    let mut search_from = 0;
    while let Some(idx) = rest[search_from..].find("---") {
        let abs = search_from + idx;
        // Must be at the start of a line.
        let at_line_start = abs == 0 || rest.as_bytes()[abs - 1] == b'\n';
        if !at_line_start {
            search_from = abs + 3;
            continue;
        }
        // Must be the entire line: followed by '\n', or '\r\n', or EOF.
        let after = &rest[abs + 3..];
        if let Some(stripped) = after.strip_prefix('\n') {
            let consumed_end = abs + 3 + (after.len() - stripped.len());
            return Some(Span {
                start: abs,
                end: consumed_end,
            });
        }
        if let Some(stripped) = after.strip_prefix("\r\n") {
            let consumed_end = abs + 3 + (after.len() - stripped.len());
            return Some(Span {
                start: abs,
                end: consumed_end,
            });
        }
        if after.is_empty() {
            return Some(Span {
                start: abs,
                end: rest.len(),
            });
        }
        search_from = abs + 3;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(body: &str) -> Result<(WorkflowMdHeader, String), WorkflowError> {
        let (h, rest) = parse_workflow_md_frontmatter(Path::new("/tmp/x.md"), body)?;
        Ok((h, rest.to_string()))
    }

    #[test]
    fn no_frontmatter_returns_default_header() {
        let (h, body) = run("# Hello\n").unwrap();
        assert_eq!(h.shape, PhaseShape::Session);
        assert!(h.stall_seconds.is_none());
        assert!(h.cli.is_none());
        assert_eq!(body, "# Hello\n");
    }

    #[test]
    fn explicit_session_command() {
        let (h, _) = run("---\nsession: \"command\"\n---\nbody\n").unwrap();
        assert_eq!(h.shape, PhaseShape::Command);
    }

    #[test]
    fn explicit_session_session() {
        let (h, _) = run("---\nsession: \"session\"\n---\nbody\n").unwrap();
        assert_eq!(h.shape, PhaseShape::Session);
    }

    #[test]
    fn invalid_session_value_is_rejected() {
        match run("---\nsession: \"bogus\"\n---\nbody\n") {
            Err(WorkflowError::InvalidSessionField { value, .. }) => {
                assert_eq!(value, "bogus");
            }
            other => panic!("expected InvalidSessionField, got {other:?}"),
        }
    }

    #[test]
    fn stall_seconds_parsed_and_validated() {
        let (h, _) = run("---\nstall_seconds: 42\n---\nbody\n").unwrap();
        assert_eq!(h.stall_seconds, Some(42));
        match run("---\nstall_seconds: 0\n---\nbody\n") {
            Err(WorkflowError::InvalidStallSeconds { value, .. }) => {
                assert_eq!(value, "0");
            }
            other => panic!("expected InvalidStallSeconds, got {other:?}"),
        }
    }

    #[test]
    fn cli_override_picked_up() {
        let (h, _) = run("---\ncli: \"claude --print\"\n---\nbody\n").unwrap();
        assert_eq!(h.cli.as_deref(), Some("claude --print"));
    }

    #[test]
    fn missing_closing_delimiter_is_error() {
        match run("---\nsession: \"session\"\nbody without closing\n") {
            Err(WorkflowError::WorkflowMdFrontmatter { reason, .. }) => {
                assert!(reason.contains("missing closing"));
            }
            other => panic!("expected WorkflowMdFrontmatter, got {other:?}"),
        }
    }

    #[test]
    fn body_after_frontmatter_returned_verbatim() {
        let (_, body) = run("---\nsession: \"session\"\n---\n# title\n\nparagraph\n").unwrap();
        assert_eq!(body, "# title\n\nparagraph\n");
    }
}
```

- [ ] **Step 4: Wire the new module**

Edit `crates/roki-daemon/src/config/mod.rs`. After the existing `pub mod` lines, add:

```rust
pub mod workflow_md;
```

- [ ] **Step 5: Run the new module's tests**

Run: `cargo test -p roki-daemon --features test-support --lib config::workflow_md`
Expected: all eight tests pass.

- [ ] **Step 6: Update slice-1 callers of `PhaseBody::Path` to provide the new fields**

Slice-1 code constructs `PhaseBody::Path { path, cli_override }` in `config/workflow.rs::parse_phase_body`, `engine/cycle.rs` tests, and `engine/phase.rs` tests. Each construction site must add `shape: PhaseShape::Session` and `stall_seconds: None` for now — Task 6 will make the loader actually populate `shape` from the frontmatter.

In `crates/roki-daemon/src/config/workflow.rs::parse_phase_body`, replace the existing `Ok(crate::engine::outcome::PhaseBody::Path { path: resolved, cli_override })` (or whatever the current shape is) with:

```rust
        Ok(crate::engine::outcome::PhaseBody::Path {
            path: resolved,
            cli_override,
            shape: crate::engine::outcome::PhaseShape::Session,
            stall_seconds: None,
        })
```

Then run `cargo build -p roki-daemon --tests` and update every test-side `PhaseBody::Path { .. }` literal the compiler complains about with the same two extra fields. Use `cargo build` as the iterative driver.

- [ ] **Step 7: Verify the whole crate still compiles and slice-1 tests pass**

Run: `cargo test -p roki-daemon --features test-support --lib`
Expected: every existing test and the eight new `workflow_md::tests` pass.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/engine/outcome.rs crates/roki-daemon/src/config/mod.rs crates/roki-daemon/src/config/workflow_md.rs crates/roki-daemon/src/config/workflow.rs
git commit -m "engine: add PhaseShape and workflow .md frontmatter parser" -m "PhaseShape distinguishes session vs command at the type level. PhaseBody::Path gains shape and stall_seconds fields, populated by the new workflow_md parser. Inline forms are variant-fixed (cmd→Command, prompt→Session) per fr:04 §29-34." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Wire `workflow_md::parse_workflow_md_frontmatter` into `parse_phase_body`

**Files:**
- Modify: `crates/roki-daemon/src/config/workflow.rs`

Slice 1's `parse_phase_body` (around `crates/roki-daemon/src/config/workflow.rs:370+`) rejects `session = ...` on every body form with `UnsupportedRunForm`. Slice 2 makes that table-driven: inline forms reject `session`, the `path` form reads `session`/`stall_seconds` from the .md file's frontmatter (not from TOML).

- [ ] **Step 1: Write a failing test for path-form frontmatter resolution**

Append to the `mod tests` block in `crates/roki-daemon/src/config/workflow.rs`:

```rust
    #[test]
    fn path_form_pulls_shape_from_md_frontmatter() {
        let dir = tempfile::TempDir::new().unwrap();
        let workflow_md = dir.path().join("foo.md");
        std::fs::write(
            &workflow_md,
            "---\nsession: \"command\"\nstall_seconds: 42\n---\nbody\n",
        )
        .unwrap();
        let workflow_toml = dir.path().join("WORKFLOW.toml");
        std::fs::write(
            &workflow_toml,
            r#"
[[rule]]
when.label = "x"

[rule.run]
path = "foo.md"
"#,
        )
        .unwrap();
        let workflow = WorkflowConfig::load(&workflow_toml).unwrap();
        let rule = &workflow.rules[0];
        match &rule.run {
            crate::engine::outcome::PhaseBody::Path {
                shape,
                stall_seconds,
                ..
            } => {
                assert_eq!(*shape, crate::engine::outcome::PhaseShape::Command);
                assert_eq!(*stall_seconds, Some(42));
            }
            other => panic!("expected PhaseBody::Path, got {other:?}"),
        }
    }

    #[test]
    fn inline_cmd_rejects_session_field() {
        let dir = tempfile::TempDir::new().unwrap();
        let workflow_toml = dir.path().join("WORKFLOW.toml");
        std::fs::write(
            &workflow_toml,
            r#"
[[rule]]
when.label = "x"

[rule.run]
cmd = "echo hi"
session = "session"
"#,
        )
        .unwrap();
        match WorkflowConfig::load(&workflow_toml) {
            Err(WorkflowError::UnsupportedRunForm { key, .. }) => {
                assert!(key.contains("session"));
            }
            other => panic!("expected UnsupportedRunForm, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the failing tests**

Run: `cargo test -p roki-daemon --features test-support --lib config::workflow::tests::path_form_pulls_shape_from_md_frontmatter`
Expected: failure — current parser sets `shape: Session` for every path form.

- [ ] **Step 3: Wire the frontmatter parser into `parse_phase_body`**

Open `crates/roki-daemon/src/config/workflow.rs`. Locate `parse_phase_body` (search for `// session = "session" is recognised but not implemented`). Make these targeted edits:

a) **Drop the slice-1 blanket rejection** of `session` on the body table. Replace the early block:

```rust
    // session = "session" is recognised but not implemented in slice 1.
    if let Some(session_val) = table.get("session") {
        let kind = session_val.as_str().unwrap_or("");
        return Err(WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: format!("{key_prefix}.session={kind}"),
        });
    }
```

with:

```rust
    let inline_session_field = table.get("session");
```

b) **Update the allow-list**: extend the recognised-keys match to include `session` (path form will consume it; inline forms will reject it below):

```rust
    for key in table.keys() {
        match key.as_str() {
            "cmd" | "prompt" | "path" | "cli" | "session" => {}
            other => {
                return Err(WorkflowError::UnsupportedRunForm {
                    path: path.to_path_buf(),
                    key: format!("{key_prefix}.{other}"),
                });
            }
        }
    }
```

c) **Reject `session` on inline forms**: insert just before the `if has_cmd` block:

```rust
    if inline_session_field.is_some() && (has_cmd || has_prompt) {
        return Err(WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: format!("{key_prefix}.session"),
        });
    }
```

d) **Handle the `path` form via the frontmatter parser**: replace the existing trailing branch (the one that reads `path`, resolves it, and constructs `PhaseBody::Path { path: resolved, cli_override }`) with:

```rust
        let resolved = resolve_workflow_path(workflow_dir, path_str);
        let toml_cli_override = table
            .get("cli")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // Slice-2: workflow .md frontmatter resolves shape + stall_seconds + cli.
        let body_text = std::fs::read_to_string(&resolved).map_err(|source| {
            WorkflowError::Unreadable {
                path: resolved.clone(),
                source,
            }
        })?;
        let (header, _post) = crate::config::workflow_md::parse_workflow_md_frontmatter(
            &resolved,
            &body_text,
        )?;

        let cli_override = toml_cli_override.or(header.cli);

        Ok(crate::engine::outcome::PhaseBody::Path {
            path: resolved,
            cli_override,
            shape: header.shape,
            stall_seconds: header.stall_seconds,
        })
```

(`Value` is `toml::Value`; the slice-1 imports already cover it.)

- [ ] **Step 4: Run all workflow tests**

Run: `cargo test -p roki-daemon --features test-support --lib config::workflow`
Expected: every existing slice-1 test plus the two new ones pass. (Slice-1 tests must still construct .md fixtures on disk for `path` cases; if any pre-existing slice-1 test fails because the .md file does not exist, fix that test by writing the .md file alongside the test — slice 2 reads the file at config load.)

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/config/workflow.rs
git commit -m "config: resolve phase shape from workflow .md frontmatter" -m "Path-form phase bodies now read session and stall_seconds from the .md file's frontmatter at config load. Inline cmd/prompt forms reject session= per fr:04 (override only via .md frontmatter)." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Implement `engine::stream` line splitter and result-event recognizer

**Files:**
- Create: `crates/roki-daemon/src/engine/stream.rs`
- Modify: `crates/roki-daemon/src/engine/mod.rs`

- [ ] **Step 1: Create the module skeleton**

Create `crates/roki-daemon/src/engine/stream.rs` with:

```rust
//! Stream-JSON line tooling.
//!
//! `LineSplitter` consumes async byte streams and yields complete lines.
//! `scan_directive_line` checks whether a parsed line carries a legal
//! `directive` value for a phase. `scan_run_terminal_line` checks whether
//! a parsed line is the claude/codex stream-json `result` event.

use std::pin::Pin;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader, Lines};

use crate::engine::outcome::{PhaseKind, PostDirective, PreDirective};

/// Lazy line iterator over a tokio async reader. Each call to `next_line`
/// returns one `\n`-terminated line (the trailing newline stripped) or `None`
/// at EOF. Lines may be of arbitrary length; tokio's BufReader does not
/// impose a length cap.
pub struct LineSplitter<R: AsyncRead + Unpin + Send> {
    inner: Lines<BufReader<R>>,
}

impl<R: AsyncRead + Unpin + Send> LineSplitter<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader).lines(),
        }
    }

    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        Pin::new(&mut self.inner).next_line().await
    }
}

/// Result of inspecting a stdout line for a phase directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectiveScan {
    /// Line was not parseable as a JSON object.
    NotJson,
    /// Parseable but does not carry a `directive` field — advisory event.
    Advisory,
    /// Has a `directive` field whose value is outside the legal set.
    SchemaDrift,
    /// Has a legal `directive` for the current phase. The full parsed value
    /// is returned so the caller can write it as the response.
    PreTerminal {
        directive: PreDirective,
        value: Value,
    },
    PostTerminal {
        directive: PostDirective,
        value: Value,
    },
}

/// Inspect a stdout line for a directive. `kind` decides which legal set
/// to validate against (Pre: run/end; Post: pre/run/end).
pub fn scan_directive_line(line: &str, kind: PhaseKind) -> DirectiveScan {
    let value: Value = match serde_json::from_str(line) {
        Ok(Value::Object(_)) => serde_json::from_str(line).unwrap(),
        _ => return DirectiveScan::NotJson,
    };

    let Some(directive_str) = value.get("directive").and_then(|v| v.as_str()) else {
        return DirectiveScan::Advisory;
    };

    match kind {
        PhaseKind::Pre => match PreDirective::try_from_str(directive_str) {
            Some(d) => DirectiveScan::PreTerminal { directive: d, value },
            None => DirectiveScan::SchemaDrift,
        },
        PhaseKind::Post => match PostDirective::try_from_str(directive_str) {
            Some(d) => DirectiveScan::PostTerminal { directive: d, value },
            None => DirectiveScan::SchemaDrift,
        },
        PhaseKind::Run => DirectiveScan::Advisory,
    }
}

/// Inspect a stdout line for the claude/codex stream-json `result` event.
/// Returns the parsed value when `type == "result"`, else `None`.
pub fn scan_run_terminal_line(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    if !value.is_object() {
        return None;
    }
    if value.get("type").and_then(|v| v.as_str()) == Some("result") {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn line_splitter_yields_lines_then_none() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        writer.write_all(b"alpha\nbeta\ngamma").await.unwrap();
        drop(writer);
        let mut split = LineSplitter::new(reader);
        assert_eq!(split.next_line().await.unwrap().unwrap(), "alpha");
        assert_eq!(split.next_line().await.unwrap().unwrap(), "beta");
        assert_eq!(split.next_line().await.unwrap().unwrap(), "gamma");
        assert_eq!(split.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn line_splitter_handles_long_line() {
        let (mut writer, reader) = tokio::io::duplex(1 << 17); // 128 KiB
        let mut huge = String::with_capacity(1 << 16);
        huge.extend(std::iter::repeat('x').take(1 << 16));
        let payload = format!("{huge}\nshort\n");
        writer.write_all(payload.as_bytes()).await.unwrap();
        drop(writer);
        let mut split = LineSplitter::new(reader);
        let big = split.next_line().await.unwrap().unwrap();
        assert_eq!(big.len(), 1 << 16);
        assert_eq!(split.next_line().await.unwrap().unwrap(), "short");
    }

    #[test]
    fn scan_directive_line_pre_terminal() {
        let scan = scan_directive_line(r#"{"directive":"run","payload":{"x":1}}"#, PhaseKind::Pre);
        match scan {
            DirectiveScan::PreTerminal { directive, value } => {
                assert_eq!(directive, PreDirective::Run);
                assert!(value.get("payload").is_some());
            }
            other => panic!("expected PreTerminal, got {other:?}"),
        }
    }

    #[test]
    fn scan_directive_line_post_terminal_end() {
        let scan = scan_directive_line(r#"{"directive":"end"}"#, PhaseKind::Post);
        assert!(matches!(
            scan,
            DirectiveScan::PostTerminal {
                directive: PostDirective::End,
                ..
            }
        ));
    }

    #[test]
    fn scan_directive_line_schema_drift() {
        let scan = scan_directive_line(r#"{"directive":"halt"}"#, PhaseKind::Post);
        assert_eq!(scan, DirectiveScan::SchemaDrift);
    }

    #[test]
    fn scan_directive_line_advisory_when_no_directive_field() {
        let scan = scan_directive_line(r#"{"type":"thinking","text":"…"}"#, PhaseKind::Post);
        assert_eq!(scan, DirectiveScan::Advisory);
    }

    #[test]
    fn scan_directive_line_not_json_for_garbage() {
        assert_eq!(scan_directive_line("not json", PhaseKind::Post), DirectiveScan::NotJson);
        assert_eq!(scan_directive_line("[1,2,3]", PhaseKind::Post), DirectiveScan::NotJson);
        assert_eq!(scan_directive_line("\"plain string\"", PhaseKind::Post), DirectiveScan::NotJson);
    }

    #[test]
    fn scan_directive_line_pre_rejects_pre_value() {
        // Pre's legal set is {run,end}. "pre" must surface as schema drift.
        let scan = scan_directive_line(r#"{"directive":"pre"}"#, PhaseKind::Pre);
        assert_eq!(scan, DirectiveScan::SchemaDrift);
    }

    #[test]
    fn scan_run_terminal_recognises_result_event() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;
        let v = scan_run_terminal_line(line).unwrap();
        assert_eq!(v.get("subtype").and_then(|v| v.as_str()), Some("success"));
    }

    #[test]
    fn scan_run_terminal_ignores_non_result_event() {
        assert!(scan_run_terminal_line(r#"{"type":"thinking"}"#).is_none());
        assert!(scan_run_terminal_line(r#"{"foo":"bar"}"#).is_none());
        assert!(scan_run_terminal_line("not json").is_none());
    }
}
```

- [ ] **Step 2: Add `pub mod stream;` to engine module**

Edit `crates/roki-daemon/src/engine/mod.rs`. Add the line in alphabetical order:

```rust
pub mod stream;
```

- [ ] **Step 3: Run the new module's tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::stream`
Expected: all eight tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/engine/stream.rs crates/roki-daemon/src/engine/mod.rs
git commit -m "engine: add stream line splitter and recognizers" -m "LineSplitter consumes async readers and yields one line per call. scan_directive_line classifies a parsed line as terminal/advisory/drift/not-json; scan_run_terminal_line recognises the claude/codex stream-json result event for run-phase capture." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Implement `engine::stall::Watchdog`

**Files:**
- Create: `crates/roki-daemon/src/engine/stall.rs`
- Modify: `crates/roki-daemon/src/engine/mod.rs`

- [ ] **Step 1: Write the module**

Create `crates/roki-daemon/src/engine/stall.rs` with:

```rust
//! Idle-stdout watchdog. Used by both `CommandPhaseExecutor` (per
//! invocation) and `SessionSupervisor` (per cycle).
//!
//! Contract: callers update `tick_stdout()` on every byte that arrives on
//! stdout. `run` polls the elapsed-since-last-byte interval and signals the
//! child if it exceeds `stall_seconds`. SIGTERM is sent first; after a
//! fixed `GRACE_PERIOD` (5 s) SIGKILL is sent if the process is still alive.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::Child;
use tokio::time::Instant;

/// Hard-coded grace period between SIGTERM and SIGKILL. Per fr:04 §126
/// ("waits up to a fixed grace period").
pub const GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Polling interval for the watchdog. 250 ms is fine-grained enough that an
/// operator stall window of `1 s` (the validated minimum) still terminates
/// the child within ~250 ms of the boundary.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Outcome of the watchdog's `run` loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallOutcome {
    /// Subprocess exited cleanly (the watchdog observed `try_wait` reporting
    /// an exit before any stall fired).
    Healthy,
    /// Stall fired; the watchdog signalled SIGTERM (and SIGKILL after grace
    /// if the child did not exit). Caller should treat the phase as
    /// `FailureKind::Stall`.
    StalledThenTerminated,
}

#[derive(Clone)]
pub struct Watchdog {
    last_stdout_ms: Arc<AtomicU64>,
    stall_seconds: Arc<AtomicU32>,
    started: Instant,
}

impl Watchdog {
    pub fn new(stall_seconds: u32) -> Self {
        Self {
            last_stdout_ms: Arc::new(AtomicU64::new(0)),
            stall_seconds: Arc::new(AtomicU32::new(stall_seconds)),
            started: Instant::now(),
        }
    }

    /// Update the last-stdout-byte timestamp. Called by the stdout reader
    /// on every byte (or every line — bytes-per-line granularity is fine
    /// because the resolution is far below `stall_seconds`).
    pub fn tick_stdout(&self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.last_stdout_ms.store(elapsed, Ordering::Relaxed);
    }

    /// Mutate the stall window mid-flight. Used by `SessionSupervisor` when
    /// the active phase carries a per-file `stall_seconds` override.
    pub fn set_stall_seconds(&self, seconds: u32) {
        self.stall_seconds.store(seconds, Ordering::Relaxed);
    }

    /// Run the watchdog until either the child exits cleanly (`Healthy`) or
    /// the stall window elapses and the watchdog terminates the child
    /// (`StalledThenTerminated`).
    pub async fn run(&self, child: &mut Child) -> StallOutcome {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.tick().await; // first tick is instant; skip
        loop {
            interval.tick().await;
            if let Ok(Some(_)) = child.try_wait() {
                return StallOutcome::Healthy;
            }

            let stall_ms = (self.stall_seconds.load(Ordering::Relaxed) as u64) * 1000;
            let elapsed_ms = self.started.elapsed().as_millis() as u64;
            let last = self.last_stdout_ms.load(Ordering::Relaxed);
            let idle_ms = elapsed_ms.saturating_sub(last);
            if idle_ms > stall_ms {
                terminate_child(child).await;
                return StallOutcome::StalledThenTerminated;
            }
        }
    }
}

async fn terminate_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    let deadline = Instant::now() + GRACE_PERIOD;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    fn sleep_child(seconds: u64) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(format!("sleep {seconds}"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sleep")
    }

    #[tokio::test(start_paused = false)]
    async fn watchdog_stalls_on_idle_child() {
        // Child sleeps 30 s but watchdog window is 1 s. The watchdog should
        // observe stall and terminate the child within ~1.3 s.
        let wd = Watchdog::new(1);
        let mut child = sleep_child(30);
        let outcome = wd.run(&mut child).await;
        assert_eq!(outcome, StallOutcome::StalledThenTerminated);
    }

    #[tokio::test(start_paused = false)]
    async fn watchdog_healthy_when_child_exits_first() {
        // Child exits in 0.2 s; window is 30 s. Watchdog must observe the
        // exit and return Healthy.
        let wd = Watchdog::new(30);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 0.2")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let outcome = wd.run(&mut child).await;
        assert_eq!(outcome, StallOutcome::Healthy);
    }

    #[tokio::test(start_paused = false)]
    async fn watchdog_resets_on_stdout_byte() {
        // Window 1 s but ticks every 200 ms keep child alive — should never stall.
        let wd = Watchdog::new(1);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 2")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let wd_clone = wd.clone();
        let ticker = tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_millis(1900);
            while Instant::now() < deadline {
                wd_clone.tick_stdout();
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        let outcome = wd.run(&mut child).await;
        let _ = ticker.await;
        assert_eq!(outcome, StallOutcome::Healthy);
    }

    #[tokio::test(start_paused = false)]
    async fn watchdog_set_stall_seconds_takes_effect() {
        // Initial window 30 s, then immediately shrink to 1 s. Child sleeps
        // 30 s, so we should observe Stall.
        let wd = Watchdog::new(30);
        let mut child = sleep_child(30);
        wd.set_stall_seconds(1);
        let outcome = wd.run(&mut child).await;
        assert_eq!(outcome, StallOutcome::StalledThenTerminated);
    }
}
```

- [ ] **Step 2: Add `pub mod stall;` to engine module**

Edit `crates/roki-daemon/src/engine/mod.rs`:

```rust
pub mod stall;
```

- [ ] **Step 3: Run the watchdog tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::stall`
Expected: all four tests pass within ~10 s wall clock total.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/engine/stall.rs crates/roki-daemon/src/engine/mod.rs
git commit -m "engine: add idle-stdout Watchdog" -m "Watchdog ticks on every stdout byte; run() polls every 250 ms and terminates the child when stall_seconds is exceeded (SIGTERM, then SIGKILL after a 5 s grace). set_stall_seconds lets the supervisor mutate the window per-turn." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Add `open_session_phase_files` and `write_run_terminal_json` to capture

**Files:**
- Modify: `crates/roki-daemon/src/capture.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `crates/roki-daemon/src/capture.rs`:

```rust
    #[test]
    fn open_session_phase_files_creates_three_files() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 1).unwrap();
        let files = open_session_phase_files(&dir, PhaseKind::Pre).unwrap();
        drop(files);
        assert!(dir.join("pre.stdout").is_file());
        assert!(dir.join("pre.stderr").is_file());
        assert!(dir.join("pre.events.jsonl").is_file());
    }

    #[test]
    fn write_run_terminal_json_writes_pretty_payload() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let value = serde_json::json!({"type":"result","is_error":false});
        write_run_terminal_json(&dir, &value).unwrap();
        let body = std::fs::read_to_string(dir.join("run.terminal.json")).unwrap();
        assert!(body.contains("\"is_error\""));
        assert!(body.contains("false"));
    }
```

- [ ] **Step 2: Run them and confirm they fail to compile**

Run: `cargo test -p roki-daemon --features test-support --lib capture::tests::open_session_phase_files_creates_three_files`
Expected: compile error — neither helper exists.

- [ ] **Step 3: Implement the two helpers**

In `crates/roki-daemon/src/capture.rs`, append the following to the file (above `#[cfg(test)] mod tests`):

```rust
/// Files opened for one session-shape phase turn. The supervisor opens these
/// at the start of `run_turn(kind, ...)` and rotates them when the next turn
/// starts.
pub struct SessionPhaseFiles {
    pub stdout: File,
    pub stderr: File,
    pub events: File,
}

/// Open `<phase>.stdout`, `<phase>.stderr`, and `<phase>.events.jsonl`
/// inside `iter_dir`. All three files are truncated on open per slice-1
/// `open_phase_files` semantics — the supervisor writes for the duration
/// of one turn and never reopens the same triple twice.
pub fn open_session_phase_files(
    iter_dir: &Path,
    phase: PhaseKind,
) -> Result<SessionPhaseFiles, CaptureError> {
    let stdout_path = iter_dir.join(format!("{}.stdout", phase.as_str()));
    let stderr_path = iter_dir.join(format!("{}.stderr", phase.as_str()));
    let events_path = iter_dir.join(format!("{}.events.jsonl", phase.as_str()));
    let stdout = File::create(&stdout_path).map_err(|source| CaptureError::OpenFile {
        path: stdout_path,
        source,
    })?;
    let stderr = File::create(&stderr_path).map_err(|source| CaptureError::OpenFile {
        path: stderr_path,
        source,
    })?;
    let events = File::create(&events_path).map_err(|source| CaptureError::OpenFile {
        path: events_path,
        source,
    })?;
    Ok(SessionPhaseFiles {
        stdout,
        stderr,
        events,
    })
}

/// Write `run.terminal.json` (pretty-printed) inside `iter_dir`. Used when
/// the run-phase tee scanner spots a claude/codex `result` event mid-stream.
pub fn write_run_terminal_json(
    iter_dir: &Path,
    value: &serde_json::Value,
) -> Result<(), CaptureError> {
    let path = iter_dir.join("run.terminal.json");
    let pretty = serde_json::to_vec_pretty(value).map_err(|err| CaptureError::Write {
        path: path.clone(),
        source: std::io::Error::other(err),
    })?;
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    file.write_all(&pretty).map_err(|source| CaptureError::Write {
        path,
        source,
    })
}
```

- [ ] **Step 4: Run the capture tests**

Run: `cargo test -p roki-daemon --features test-support --lib capture`
Expected: every test (existing + 2 new) passes.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/capture.rs
git commit -m "capture: add session and run-terminal helpers" -m "open_session_phase_files opens the (stdout, stderr, events.jsonl) triple for one session-shape phase turn. write_run_terminal_json materialises the claude/codex result event detected by the tee scanner." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Rewrite `CommandPhaseExecutor::execute` to tee stdout via a tokio task

**Files:**
- Modify: `crates/roki-daemon/src/engine/phase.rs`

The slice-1 executor pipes stdout directly to a `File` via `Stdio::from(File)`. Slice 2 needs the bytes to flow through a tokio task so the stall watchdog can `tick_stdout` per byte and (Task 11) the run scanner can extract `run.terminal.json`.

- [ ] **Step 1: Write a failing test for the watchdog integration**

Append to `mod tests` in `crates/roki-daemon/src/engine/phase.rs`:

```rust
    #[tokio::test]
    async fn command_phase_stalls_on_idle_child() {
        // Run a command that sleeps with no stdout. With stall_seconds = 1
        // the executor must surface FailureKind::Stall.
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir = crate::capture::create_iter_dir(
            tmp.path(),
            "ENG-1",
            uuid::Uuid::nil(),
            1,
        )
        .unwrap();
        let body = crate::engine::outcome::PhaseBody::InlineCmd {
            cmd: "sleep 30".to_string(),
        };
        let exec = CommandPhaseExecutor::new(StallWindow::Override(1));
        let ctx = test_context();
        let outcome = exec
            .execute(crate::engine::outcome::PhaseKind::Run, &body, &ctx, &iter_dir)
            .await
            .unwrap();
        match outcome {
            crate::engine::outcome::PhaseOutcome::Failure {
                kind: crate::engine::outcome::FailureKind::Stall,
            } => {}
            other => panic!("expected Failure(Stall), got {other:?}"),
        }
    }
```

(`test_context()` is the existing slice-1 helper in the same `mod tests`. `StallWindow::Override(1)` is introduced in Step 3 below; if your slice-1 executor takes a different shape, adapt the call accordingly.)

- [ ] **Step 2: Run it and confirm it fails to compile**

Run: `cargo test -p roki-daemon --features test-support --lib engine::phase::tests::command_phase_stalls_on_idle_child`
Expected: compile error — `StallWindow::Override` does not yet exist; the executor does not yet integrate the watchdog.

- [ ] **Step 3: Add a `StallWindow` config struct**

Near the top of `crates/roki-daemon/src/engine/phase.rs`, add:

```rust
/// Stall window resolution at construction time. Lets the executor honour
/// either the shape default or a per-file override without re-reading
/// config in the hot path.
#[derive(Debug, Clone, Copy)]
pub enum StallWindow {
    /// Use this many seconds.
    Override(u32),
    /// Use the command-shape default from `[default.ai.command].stall_seconds`.
    CommandDefault(u32),
}

impl StallWindow {
    pub fn seconds(self) -> u32 {
        match self {
            StallWindow::Override(n) | StallWindow::CommandDefault(n) => n,
        }
    }
}
```

Modify `CommandPhaseExecutor` to carry the watchdog window. Replace the existing struct + `new` (or wherever the executor is constructed) with:

```rust
pub struct CommandPhaseExecutor {
    stall: StallWindow,
}

impl CommandPhaseExecutor {
    pub fn new(stall: StallWindow) -> Self {
        Self { stall }
    }
}
```

(If slice-1 had `CommandPhaseExecutor::default()`, replace its callers with `CommandPhaseExecutor::new(StallWindow::CommandDefault(300))` for now. Task 17 wires the real config.)

- [ ] **Step 4: Replace the spawn body with a tee'd stdout pipeline**

Locate the body of `CommandPhaseExecutor::execute` in `crates/roki-daemon/src/engine/phase.rs`. The slice-1 spawn-and-redirect block (around the `Command::new` + `.stdout(File::from(...))` calls) becomes:

```rust
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command as TokioCommand;
        use std::process::Stdio;

        let stdout_path = iter_dir.join(format!("{}.stdout", kind.as_str()));
        let stderr_path = iter_dir.join(format!("{}.stderr", kind.as_str()));

        // Open the on-disk stdout/stderr files via the existing helper.
        let (stdout_file, stderr_file) = crate::capture::open_phase_files(iter_dir, kind)?;

        // We tee bytes via tokio tasks; spawn with piped stdio.
        let mut child = TokioCommand::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .envs(env_pairs)
            .current_dir(&cwd)
            .stdin(if has_stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| PhaseInfraError::Spawn {
                cmd: argv[0].clone(),
                source,
            })?;

        if let Some(body_bytes) = stdin_body.as_ref() {
            let mut stdin = child.stdin.take().ok_or_else(|| PhaseInfraError::StdinUnavailable {
                cmd: argv[0].clone(),
            })?;
            stdin
                .write_all(body_bytes)
                .await
                .map_err(|source| PhaseInfraError::StdinWrite {
                    cmd: argv[0].clone(),
                    source,
                })?;
            drop(stdin);
        }

        let stdout_pipe = child.stdout.take().expect("piped");
        let stderr_pipe = child.stderr.take().expect("piped");

        let watchdog = Watchdog::new(self.stall.seconds());

        let stdout_handle = {
            let wd = watchdog.clone();
            let raw = stdout_file;
            let iter_dir_run_terminal = iter_dir.to_path_buf();
            let kind_for_task = kind;
            tokio::spawn(async move {
                tee_stdout(stdout_pipe, raw, wd, kind_for_task, iter_dir_run_terminal).await
            })
        };

        let stderr_handle = {
            let mut raw = stderr_file;
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                use tokio::io::AsyncReadExt;
                let mut reader = stderr_pipe;
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            use std::io::Write;
                            let _ = raw.write_all(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let stall_outcome = watchdog.run(&mut child).await;
        let _ = stdout_handle.await;
        let _ = stderr_handle.await;
        let exit_status = child.wait().await.map_err(|source| PhaseInfraError::Wait {
            cmd: argv[0].clone(),
            source,
        })?;

        if stall_outcome == StallOutcome::StalledThenTerminated {
            return Ok(PhaseOutcome::Failure {
                kind: FailureKind::Stall,
            });
        }

        // Slice-1 directive scan / run.exit_code path resumes from here.
        // The slice-1 module body that scanned the on-disk stdout file and
        // wrote response.json / run.exit_code is unchanged below this block.
```

Add `tee_stdout` as a private helper at the bottom of the file:

```rust
async fn tee_stdout(
    mut stdout_pipe: tokio::process::ChildStdout,
    mut raw_writer: std::fs::File,
    watchdog: crate::engine::stall::Watchdog,
    _kind: crate::engine::outcome::PhaseKind,
    _iter_dir: std::path::PathBuf,
) {
    use std::io::Write;
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    loop {
        match stdout_pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                watchdog.tick_stdout();
                let _ = raw_writer.write_all(&buf[..n]);
            }
            Err(_) => break,
        }
    }
}
```

(The `_kind` and `_iter_dir` arguments stay unused for now; Task 11 wires them to call `scan_run_terminal_line` and `write_run_terminal_json`.)

Add the missing imports near the top of `phase.rs`:

```rust
use crate::engine::stall::{StallOutcome, Watchdog};
```

- [ ] **Step 5: Update slice-1 callers of `CommandPhaseExecutor::new()`**

Run `cargo build -p roki-daemon --tests` and add `StallWindow::CommandDefault(300)` to every site the compiler complains about (tests, `runtime::run_inner`, etc.).

- [ ] **Step 6: Run the new test plus the slice-1 phase tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::phase`
Expected: every existing test plus the new `command_phase_stalls_on_idle_child` passes.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/src/engine/phase.rs
git commit -m "engine: tee command-phase stdout and integrate watchdog" -m "Slice 1's direct file-redirect is replaced by a tokio tee task that ticks the watchdog per byte. The watchdog runs concurrently with child.wait() and surfaces FailureKind::Stall on idle stdout. run.terminal.json extraction is wired in the next task." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Extract `run.terminal.json` mid-stream during `Run` phase

**Files:**
- Modify: `crates/roki-daemon/src/engine/phase.rs`

- [ ] **Step 1: Write a failing test**

Append to `mod tests` in `crates/roki-daemon/src/engine/phase.rs`:

```rust
    #[tokio::test]
    async fn run_phase_extracts_terminal_result_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir = crate::capture::create_iter_dir(
            tmp.path(),
            "ENG-1",
            uuid::Uuid::nil(),
            1,
        )
        .unwrap();
        let body = crate::engine::outcome::PhaseBody::InlineCmd {
            cmd: r#"printf '%s\n' '{"type":"thinking"}' '{"type":"result","is_error":false,"result":"ok"}'"#
                .to_string(),
        };
        let exec = CommandPhaseExecutor::new(StallWindow::CommandDefault(30));
        let ctx = test_context();
        let outcome = exec
            .execute(crate::engine::outcome::PhaseKind::Run, &body, &ctx, &iter_dir)
            .await
            .unwrap();
        match outcome {
            crate::engine::outcome::PhaseOutcome::RunDone { exit_code, .. } => {
                assert_eq!(exit_code, 0);
            }
            other => panic!("expected RunDone, got {other:?}"),
        }
        let body = std::fs::read_to_string(iter_dir.join("run.terminal.json")).unwrap();
        assert!(body.contains("\"is_error\""));
        assert!(body.contains("\"result\""));
    }

    #[tokio::test]
    async fn run_phase_omits_terminal_when_no_result_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir = crate::capture::create_iter_dir(
            tmp.path(),
            "ENG-1",
            uuid::Uuid::nil(),
            1,
        )
        .unwrap();
        let body = crate::engine::outcome::PhaseBody::InlineCmd {
            cmd: "echo plain".to_string(),
        };
        let exec = CommandPhaseExecutor::new(StallWindow::CommandDefault(30));
        let ctx = test_context();
        let _ = exec
            .execute(crate::engine::outcome::PhaseKind::Run, &body, &ctx, &iter_dir)
            .await
            .unwrap();
        assert!(!iter_dir.join("run.terminal.json").exists());
    }
```

- [ ] **Step 2: Run them and confirm the first one fails**

Run: `cargo test -p roki-daemon --features test-support --lib engine::phase::tests::run_phase_extracts_terminal_result_event`
Expected: failure — `run.terminal.json` is not created yet.

- [ ] **Step 3: Wire the scanner inside the tee task**

Replace the `tee_stdout` helper in `crates/roki-daemon/src/engine/phase.rs` with:

```rust
async fn tee_stdout(
    stdout_pipe: tokio::process::ChildStdout,
    mut raw_writer: std::fs::File,
    watchdog: crate::engine::stall::Watchdog,
    kind: crate::engine::outcome::PhaseKind,
    iter_dir: std::path::PathBuf,
) {
    use std::io::Write;
    use crate::engine::stream::{scan_run_terminal_line, LineSplitter};

    let mut splitter = LineSplitter::new(stdout_pipe);
    let mut terminal_written = false;

    loop {
        let line_res = splitter.next_line().await;
        watchdog.tick_stdout();
        match line_res {
            Ok(Some(line)) => {
                let _ = raw_writer.write_all(line.as_bytes());
                let _ = raw_writer.write_all(b"\n");
                if matches!(kind, crate::engine::outcome::PhaseKind::Run) && !terminal_written {
                    if let Some(value) = scan_run_terminal_line(&line) {
                        if let Err(err) = crate::capture::write_run_terminal_json(&iter_dir, &value)
                        {
                            tracing::warn!(target: "roki.engine.run_terminal", error = ?err, "run.terminal.json write failed");
                        } else {
                            terminal_written = true;
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}
```

- [ ] **Step 4: Run the run-phase tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::phase`
Expected: every existing test plus the two new ones pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/phase.rs
git commit -m "engine: extract run.terminal.json mid-stream" -m "Run-phase tee task scans every line for a claude/codex result event. The first match is pretty-printed to <iter>/run.terminal.json without waiting for child exit; later lines stay in run.stdout. Pre/post phases skip the scan." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: Implement `SessionSupervisor::spawn` and the reader task scaffold

**Files:**
- Create: `crates/roki-daemon/src/engine/session.rs`
- Modify: `crates/roki-daemon/src/engine/mod.rs`

This task installs only the spawn path and the reader task that drains stdout into `events.jsonl`. `run_turn` and `shutdown` are added in Tasks 13–14.

- [ ] **Step 1: Create the session module**

Create `crates/roki-daemon/src/engine/session.rs`:

```rust
//! Long-lived session subprocess for slice-2 session-shape phases.
//!
//! `SessionSupervisor::spawn` constructs the child once per cycle. The
//! reader task drains stdout line-by-line and routes each line through:
//! - `events.jsonl` (parseable JSON only, per fr:04 §72)
//! - `<phase>.stdout` (every line, raw)
//! - directive channel (the first line whose `directive` field is legal
//!   for the active phase becomes the turn terminal)
//!
//! `run_turn` (Task 13) writes a rendered body to the child's stdin and
//! waits for the directive channel.
//!
//! `shutdown` (Task 14) closes stdin, waits the stall window, and SIGTERMs
//! / SIGKILLs as needed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, watch, Mutex};

use crate::capture::{open_session_phase_files, SessionPhaseFiles};
use crate::engine::outcome::{PhaseKind, PhaseShape};
use crate::engine::stall::Watchdog;
use crate::error::PhaseInfraError;

/// Configuration for `SessionSupervisor::spawn`.
pub struct SessionConfig {
    pub cli: String,                          // already Liquid-rendered
    pub argv: Vec<String>,                    // shell-words split of cli
    pub default_stall_seconds: u32,
    pub cwd: PathBuf,
    pub envs: Vec<(String, String)>,          // ROKI_* + PATH/HOME/USER passthrough
}

/// One event the reader task pushes onto the directive channel.
#[derive(Debug)]
pub enum SessionEvent {
    /// Reader observed a legal directive line. The full parsed value is
    /// included so `run_turn` can write `<phase>.response.json`.
    Directive { value: Value },
    /// Reader saw a line whose `directive` value was outside the legal set
    /// for the active phase.
    SchemaDrift,
    /// Stdout closed (the child exited or the pipe broke).
    Exit,
}

/// Per-turn state shared between `run_turn` and the reader task.
#[derive(Debug, Clone)]
struct TurnState {
    /// Phase position (Pre / Post). The reader uses this to decide which
    /// directive set to validate against.
    kind: PhaseKind,
    /// Generation counter so the reader ignores directive lines that
    /// arrived before the current turn started.
    generation: u64,
}

pub struct SessionSupervisor {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    files: Arc<Mutex<Option<SessionPhaseFiles>>>,
    turn: watch::Sender<TurnState>,
    dir_rx: Mutex<mpsc::Receiver<SessionEvent>>,
    watchdog: Watchdog,
    default_stall_seconds: u32,
}

impl SessionSupervisor {
    /// Spawn the long-lived child plus the reader task. The supervisor is
    /// idle (no turn active) until `run_turn` is called.
    pub async fn spawn(cfg: SessionConfig) -> Result<Self, PhaseInfraError> {
        use std::process::Stdio;
        use tokio::process::Command as TokioCommand;

        if cfg.argv.is_empty() {
            return Err(PhaseInfraError::SessionCliMissing);
        }

        let mut envs = std::collections::HashMap::new();
        for (k, v) in cfg.envs.iter() {
            envs.insert(k.clone(), v.clone());
        }

        let mut child = TokioCommand::new(&cfg.argv[0])
            .args(&cfg.argv[1..])
            .env_clear()
            .envs(envs)
            .current_dir(&cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| PhaseInfraError::SessionSpawn {
                cli: cfg.cli.clone(),
                source,
            })?;

        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let watchdog = Watchdog::new(cfg.default_stall_seconds);
        let files: Arc<Mutex<Option<SessionPhaseFiles>>> = Arc::new(Mutex::new(None));
        let (turn_tx, turn_rx) = watch::channel(TurnState {
            kind: PhaseKind::Pre,
            generation: 0,
        });
        let (dir_tx, dir_rx) = mpsc::channel(8);

        // Stdout reader task.
        {
            let watchdog = watchdog.clone();
            let files = files.clone();
            let turn_rx = turn_rx.clone();
            tokio::spawn(reader_task(stdout, watchdog, files, turn_rx, dir_tx));
        }
        // Stderr drain task — implemented in Task 15.
        {
            // For now, just drain into the active turn's stderr file when
            // present, otherwise discard. The between-turn buffer is added
            // in Task 15.
            let files = files.clone();
            tokio::spawn(stderr_drain_task(stderr, files));
        }

        Ok(Self {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            files,
            turn: turn_tx,
            dir_rx: Mutex::new(dir_rx),
            watchdog,
            default_stall_seconds: cfg.default_stall_seconds,
        })
    }

    /// Open `<phase>.{stdout,stderr,events.jsonl}` for the upcoming turn and
    /// activate them on the reader / drain tasks. Bumps the generation so
    /// stale directive events from a previous turn are ignored.
    pub async fn begin_turn(
        &self,
        iter_dir: &Path,
        kind: PhaseKind,
    ) -> Result<u64, PhaseInfraError> {
        let triple = open_session_phase_files(iter_dir, kind)?;
        let mut guard = self.files.lock().await;
        *guard = Some(triple);
        let new_state = {
            let prev = self.turn.borrow();
            TurnState {
                kind,
                generation: prev.generation + 1,
            }
        };
        let gen = new_state.generation;
        let _ = self.turn.send(new_state);
        Ok(gen)
    }
}

async fn reader_task(
    stdout: tokio::process::ChildStdout,
    watchdog: Watchdog,
    files: Arc<Mutex<Option<SessionPhaseFiles>>>,
    mut turn_rx: watch::Receiver<TurnState>,
    dir_tx: mpsc::Sender<SessionEvent>,
) {
    use std::io::Write;
    use crate::engine::stream::{scan_directive_line, DirectiveScan, LineSplitter};

    let mut splitter = LineSplitter::new(stdout);
    let mut last_emitted_generation: u64 = 0;

    loop {
        let line_res = splitter.next_line().await;
        watchdog.tick_stdout();
        match line_res {
            Ok(Some(line)) => {
                // Snapshot the active turn state so the rest of the loop
                // body uses a consistent (kind, generation) pair.
                let state = turn_rx.borrow().clone();

                // Always tee to <phase>.stdout when files are open.
                if let Some(triple) = files.lock().await.as_mut() {
                    let _ = triple.stdout.write_all(line.as_bytes());
                    let _ = triple.stdout.write_all(b"\n");
                }

                // events.jsonl: parseable lines only.
                let scan = scan_directive_line(&line, state.kind);
                let parseable = !matches!(scan, DirectiveScan::NotJson);
                if parseable {
                    if let Some(triple) = files.lock().await.as_mut() {
                        let _ = triple.events.write_all(line.as_bytes());
                        let _ = triple.events.write_all(b"\n");
                    }
                }

                // Directive channel: first legal directive per turn-generation.
                if state.generation > last_emitted_generation {
                    match scan {
                        DirectiveScan::PreTerminal { value, .. }
                        | DirectiveScan::PostTerminal { value, .. } => {
                            if dir_tx.send(SessionEvent::Directive { value }).await.is_err() {
                                break;
                            }
                            last_emitted_generation = state.generation;
                        }
                        DirectiveScan::SchemaDrift => {
                            if dir_tx.send(SessionEvent::SchemaDrift).await.is_err() {
                                break;
                            }
                            last_emitted_generation = state.generation;
                        }
                        _ => {}
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let _ = dir_tx.send(SessionEvent::Exit).await;
}

async fn stderr_drain_task(
    mut stderr: tokio::process::ChildStderr,
    files: Arc<Mutex<Option<SessionPhaseFiles>>>,
) {
    use std::io::Write;
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Some(triple) = files.lock().await.as_mut() {
                    let _ = triple.stderr.write_all(&buf[..n]);
                }
            }
            Err(_) => break,
        }
    }
}
```

- [ ] **Step 2: Add `pub mod session;` to engine module**

Edit `crates/roki-daemon/src/engine/mod.rs`:

```rust
pub mod session;
```

Re-export the supervisor in the same file:

```rust
pub use session::{SessionConfig, SessionSupervisor};
```

- [ ] **Step 3: Add a smoke test for `spawn` + `begin_turn`**

Append to `mod tests` in `crates/roki-daemon/src/engine/session.rs` (create the `mod tests` block if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::create_iter_dir;
    use uuid::Uuid;

    fn echo_session_cfg() -> SessionConfig {
        SessionConfig {
            cli: "cat".to_string(),
            argv: vec!["cat".to_string()],
            default_stall_seconds: 5,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn spawn_creates_child_and_pipes() {
        let sup = SessionSupervisor::spawn(echo_session_cfg()).await.unwrap();
        // child should be alive
        let mut child_guard = sup.child.lock().await;
        let child = child_guard.as_mut().unwrap();
        assert!(child.try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn begin_turn_opens_three_files() {
        let sup = SessionSupervisor::spawn(echo_session_cfg()).await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir = create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 1).unwrap();
        let _gen = sup.begin_turn(&iter_dir, PhaseKind::Pre).await.unwrap();
        assert!(iter_dir.join("pre.stdout").is_file());
        assert!(iter_dir.join("pre.stderr").is_file());
        assert!(iter_dir.join("pre.events.jsonl").is_file());
    }
}
```

- [ ] **Step 4: Run the new tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::session`
Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/session.rs crates/roki-daemon/src/engine/mod.rs
git commit -m "engine: scaffold SessionSupervisor with reader task" -m "Spawns the long-lived child once per cycle, drains stdout via tokio reader task, and exposes begin_turn to swap per-turn capture files. Run-turn / shutdown follow in subsequent commits." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Implement `SessionSupervisor::run_turn`

**Files:**
- Modify: `crates/roki-daemon/src/engine/session.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `crates/roki-daemon/src/engine/session.rs`:

```rust
    /// Bash fake AI: reads stdin lines and emits a directive object on stdout
    /// per stdin line. We use it to verify run_turn end-to-end.
    fn fake_session_cfg() -> SessionConfig {
        let script = r#"
while IFS= read -r line; do
  printf '{"type":"thinking"}\n'
  printf '{"directive":"end","echo":"%s"}\n' "$line"
done
"#;
        SessionConfig {
            cli: "bash".to_string(),
            argv: vec!["bash".to_string(), "-c".to_string(), script.to_string()],
            default_stall_seconds: 5,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn run_turn_returns_post_directive_end() {
        let sup = SessionSupervisor::spawn(fake_session_cfg()).await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let iter_dir = create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 1).unwrap();
        let outcome = sup
            .run_turn(&iter_dir, PhaseKind::Post, b"hello\n", None)
            .await
            .unwrap();
        match outcome {
            crate::engine::outcome::PhaseOutcome::PostDirective { directive, payload } => {
                assert_eq!(directive, crate::engine::outcome::PostDirective::End);
                assert_eq!(payload.get("echo").and_then(|v| v.as_str()), Some("hello"));
            }
            other => panic!("expected PostDirective(End), got {other:?}"),
        }
        let events = std::fs::read_to_string(iter_dir.join("post.events.jsonl")).unwrap();
        assert!(events.contains("\"thinking\""));
        assert!(events.contains("\"end\""));
        assert!(iter_dir.join("post.response.json").is_file());
    }
```

- [ ] **Step 2: Run it and confirm it fails to compile**

Run: `cargo test -p roki-daemon --features test-support --lib engine::session::tests::run_turn_returns_post_directive_end`
Expected: compile error — `SessionSupervisor::run_turn` does not exist.

- [ ] **Step 3: Implement `run_turn`**

Add the following method inside `impl SessionSupervisor` in `crates/roki-daemon/src/engine/session.rs`:

```rust
    /// Drive one turn end-to-end:
    ///   1. open the per-turn capture triple,
    ///   2. write `body_bytes` to the child's stdin (no close),
    ///   3. await a directive event from the reader task,
    ///   4. write `<phase>.response.json` and return `PhaseOutcome`.
    ///
    /// `stall_override` lets the cycle apply a `PhaseBody::Path::stall_seconds`
    /// override for the turn; the supervisor reverts to the default after.
    pub async fn run_turn(
        &self,
        iter_dir: &Path,
        kind: PhaseKind,
        body_bytes: &[u8],
        stall_override: Option<u32>,
    ) -> Result<crate::engine::outcome::PhaseOutcome, PhaseInfraError> {
        use crate::engine::outcome::{PhaseOutcome, PostDirective, PreDirective, FailureKind};

        let _ = self.begin_turn(iter_dir, kind).await?;

        if let Some(seconds) = stall_override {
            self.watchdog.set_stall_seconds(seconds);
        } else {
            self.watchdog.set_stall_seconds(self.default_stall_seconds);
        }

        // Write body to stdin — keep stdin open across turns.
        {
            let mut stdin_guard = self.stdin.lock().await;
            let stdin = stdin_guard.as_mut().ok_or(PhaseInfraError::SessionStdinClosed { phase: kind })?;
            stdin
                .write_all(body_bytes)
                .await
                .map_err(|_| PhaseInfraError::SessionStdinClosed { phase: kind })?;
            stdin
                .flush()
                .await
                .map_err(|_| PhaseInfraError::SessionStdinClosed { phase: kind })?;
        }

        // Await directive (or schema drift, or exit).
        let mut rx_guard = self.dir_rx.lock().await;
        let event = rx_guard.recv().await;

        match event {
            Some(SessionEvent::Directive { value }) => {
                crate::capture::write_response_json(iter_dir, kind, &value)?;
                let directive_str = value
                    .get("directive")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                match kind {
                    PhaseKind::Pre => {
                        let dir = PreDirective::try_from_str(directive_str)
                            .ok_or(PhaseInfraError::SessionStdoutClosed { phase: kind })?;
                        Ok(PhaseOutcome::PreDirective {
                            directive: dir,
                            payload: value,
                        })
                    }
                    PhaseKind::Post => {
                        let dir = PostDirective::try_from_str(directive_str)
                            .ok_or(PhaseInfraError::SessionStdoutClosed { phase: kind })?;
                        Ok(PhaseOutcome::PostDirective {
                            directive: dir,
                            payload: value,
                        })
                    }
                    PhaseKind::Run => Err(PhaseInfraError::ExecutorContract {
                        phase: kind,
                        got_variant: "PreDirective/PostDirective on Run",
                        iter: 0,
                    }),
                }
            }
            Some(SessionEvent::SchemaDrift) => Ok(PhaseOutcome::Failure {
                kind: FailureKind::SchemaDrift,
            }),
            Some(SessionEvent::Exit) => Ok(PhaseOutcome::Failure {
                kind: FailureKind::ProcessCrash,
            }),
            None => Err(PhaseInfraError::SessionStdoutClosed { phase: kind }),
        }
    }
```

- [ ] **Step 4: Run the new test plus the prior session tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::session`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/session.rs
git commit -m "engine: implement SessionSupervisor::run_turn" -m "One turn = open per-phase capture triple, write body to stdin, await directive event from reader task, write response.json. Schema drift and unexpected exit surface as PhaseOutcome::Failure." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 14: Implement `SessionSupervisor::shutdown`

**Files:**
- Modify: `crates/roki-daemon/src/engine/session.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `crates/roki-daemon/src/engine/session.rs`:

```rust
    #[tokio::test]
    async fn shutdown_completed_closes_stdin_and_waits_for_clean_exit() {
        // The fake_session_cfg loop exits as soon as stdin closes, so a
        // Completed shutdown should observe a clean exit without SIGTERM.
        let sup = SessionSupervisor::spawn(fake_session_cfg()).await.unwrap();
        sup.shutdown(SessionShutdownReason::Completed).await;
        // Subsequent shutdown is a no-op.
        sup.shutdown(SessionShutdownReason::Completed).await;
    }

    #[tokio::test]
    async fn shutdown_iter_exhausted_terminates_after_stall_window() {
        // Use a child that ignores stdin close. Shutdown must SIGTERM after
        // the stall window (here 1 s).
        let cfg = SessionConfig {
            cli: "sh -c".to_string(),
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                "trap '' TERM; sleep 30".to_string(),
            ],
            default_stall_seconds: 1,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        };
        let sup = SessionSupervisor::spawn(cfg).await.unwrap();
        let started = std::time::Instant::now();
        sup.shutdown(SessionShutdownReason::IterExhausted).await;
        // SIGTERM after 1 s + 5 s grace + SIGKILL — should finish well before 30 s.
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
    }
```

- [ ] **Step 2: Run them and confirm they fail to compile**

Run: `cargo test -p roki-daemon --features test-support --lib engine::session::tests::shutdown_completed_closes_stdin_and_waits_for_clean_exit`
Expected: compile error — `SessionSupervisor::shutdown` and `SessionShutdownReason` do not exist.

- [ ] **Step 3: Implement shutdown**

Add the following to `crates/roki-daemon/src/engine/session.rs` (after the `impl SessionSupervisor` block, or at the bottom of the existing one):

```rust
/// Reason the cycle is asking the supervisor to wind down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionShutdownReason {
    /// Cycle ended via terminal directive — child should exit cleanly when
    /// stdin closes.
    Completed,
    /// `iter == max_iterations` and post returned `pre`/`run`. Per fr:01
    /// §123-125: close stdin, wait the stall window, SIGTERM if still alive.
    IterExhausted,
    /// Earlier failure on a phase. Child may be partially through a turn;
    /// terminate without waiting on stdin.
    Failed,
}

impl SessionSupervisor {
    pub async fn shutdown(&self, reason: SessionShutdownReason) {
        use std::time::Duration;
        use tokio::time::Instant;

        // Close stdin first (Completed / IterExhausted want a graceful exit).
        if !matches!(reason, SessionShutdownReason::Failed) {
            let mut stdin_guard = self.stdin.lock().await;
            *stdin_guard = None; // drop the writer, stdin EOFs
        }

        // Wait up to default_stall_seconds for the child to exit on its own.
        let deadline = Instant::now() + Duration::from_secs(self.default_stall_seconds as u64);
        loop {
            let mut child_guard = self.child.lock().await;
            let Some(child) = child_guard.as_mut() else {
                return;
            };
            if child.try_wait().ok().flatten().is_some() {
                *child_guard = None;
                return;
            }
            drop(child_guard);
            if Instant::now() >= deadline || matches!(reason, SessionShutdownReason::Failed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Stall window expired (or Failed reason) — SIGTERM, grace, SIGKILL.
        let mut child_guard = self.child.lock().await;
        let Some(child) = child_guard.as_mut() else {
            return;
        };
        crate::engine::stall::terminate_child_external(child).await;
        *child_guard = None;
    }
}
```

(`terminate_child_external` is a thin pub-crate-export of the slice-2 `terminate_child` helper — add it to `engine::stall.rs`.)

In `crates/roki-daemon/src/engine/stall.rs`, change the existing `async fn terminate_child(...)` to `pub(crate)` and rename the public alias:

```rust
pub(crate) async fn terminate_child(child: &mut Child) {
    // (existing body unchanged)
}

/// Public alias usable from `engine::session`.
pub async fn terminate_child_external(child: &mut Child) {
    terminate_child(child).await
}
```

- [ ] **Step 4: Run the shutdown tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::session::tests::shutdown_completed_closes_stdin_and_waits_for_clean_exit engine::session::tests::shutdown_iter_exhausted_terminates_after_stall_window`
Expected: both pass within ~10 s wall clock.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/session.rs crates/roki-daemon/src/engine/stall.rs
git commit -m "engine: SessionSupervisor shutdown sequence" -m "Completed / IterExhausted close stdin and wait the stall window before SIGTERM. Failed skips the wait. Shutdown is idempotent so the cycle can call it on every exit path." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 15: Add the between-turn stderr buffer to `SessionSupervisor`

**Files:**
- Modify: `crates/roki-daemon/src/engine/session.rs`

Spec §4.4: between-turn stderr bytes (after a directive arrives, before the next `run_turn` opens its file) accumulate in supervisor RAM, capped at 64 KiB, and flush into the next turn's `<phase>.stderr`.

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `crates/roki-daemon/src/engine/session.rs`:

```rust
    #[tokio::test]
    async fn between_turn_stderr_flushes_into_next_turn() {
        // Fake AI:
        //   turn 1: emits "{ \"directive\": \"end\" }" then sleeps 200 ms
        //           emitting a stderr line.
        //   When stdin re-opens for turn 2: emits another directive.
        let script = r#"
emit_turn() {
  printf '{"directive":"end","tag":"%s"}\n' "$1"
  printf 'between-turn-line\n' >&2
}
read -r _line1
emit_turn t1
sleep 0.2
read -r _line2
emit_turn t2
"#;
        let cfg = SessionConfig {
            cli: "bash".to_string(),
            argv: vec!["bash".to_string(), "-c".to_string(), script.to_string()],
            default_stall_seconds: 5,
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
        };
        let sup = SessionSupervisor::spawn(cfg).await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let iter1 = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let _ = sup
            .run_turn(&iter1, PhaseKind::Post, b"go1\n", None)
            .await
            .unwrap();
        // Give the script time to write the between-turn stderr line.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let iter2 = create_iter_dir(tmp.path(), "X", Uuid::nil(), 2).unwrap();
        let _ = sup
            .run_turn(&iter2, PhaseKind::Post, b"go2\n", None)
            .await
            .unwrap();
        sup.shutdown(SessionShutdownReason::Completed).await;

        let iter2_stderr = std::fs::read_to_string(iter2.join("post.stderr")).unwrap();
        assert!(
            iter2_stderr.contains("between-turn-line"),
            "iter2/post.stderr should contain the bytes that arrived between turns: {iter2_stderr:?}"
        );
    }
```

- [ ] **Step 2: Implement the buffer**

Edit `crates/roki-daemon/src/engine/session.rs`. Replace the `stderr_drain_task` body and add a buffer field on `SessionSupervisor`:

```rust
const SESSION_BETWEEN_TURN_STDERR_CAP: usize = 64 * 1024;

struct StderrBuf {
    bytes: Vec<u8>,
    truncated: bool,
}
```

Extend `SessionSupervisor` to carry the buffer:

```rust
pub struct SessionSupervisor {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    files: Arc<Mutex<Option<SessionPhaseFiles>>>,
    turn: watch::Sender<TurnState>,
    dir_rx: Mutex<mpsc::Receiver<SessionEvent>>,
    watchdog: Watchdog,
    default_stall_seconds: u32,
    between_turn_stderr: Arc<Mutex<StderrBuf>>,
}
```

Initialise it in `spawn` (replace the relevant block):

```rust
        let between_turn_stderr = Arc::new(Mutex::new(StderrBuf {
            bytes: Vec::new(),
            truncated: false,
        }));

        // Stderr drain task with between-turn buffering.
        {
            let files = files.clone();
            let buf = between_turn_stderr.clone();
            tokio::spawn(stderr_drain_task(stderr, files, buf));
        }

        Ok(Self {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            files,
            turn: turn_tx,
            dir_rx: Mutex::new(dir_rx),
            watchdog,
            default_stall_seconds: cfg.default_stall_seconds,
            between_turn_stderr,
        })
```

Replace the body of `stderr_drain_task`:

```rust
async fn stderr_drain_task(
    mut stderr: tokio::process::ChildStderr,
    files: Arc<Mutex<Option<SessionPhaseFiles>>>,
    buf: Arc<Mutex<StderrBuf>>,
) {
    use std::io::Write;
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let mut files_guard = files.lock().await;
                if let Some(triple) = files_guard.as_mut() {
                    let _ = triple.stderr.write_all(&chunk[..n]);
                } else {
                    drop(files_guard);
                    let mut buf_guard = buf.lock().await;
                    let remaining = SESSION_BETWEEN_TURN_STDERR_CAP.saturating_sub(buf_guard.bytes.len());
                    let take = remaining.min(n);
                    buf_guard.bytes.extend_from_slice(&chunk[..take]);
                    if take < n {
                        if !buf_guard.truncated {
                            tracing::warn!(
                                target: "roki.engine.session",
                                cap = SESSION_BETWEEN_TURN_STDERR_CAP,
                                "phase_stderr_truncated"
                            );
                            buf_guard.truncated = true;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
}
```

Modify `begin_turn` to flush the buffer before activating the new files. Replace the existing `begin_turn` body:

```rust
    pub async fn begin_turn(
        &self,
        iter_dir: &Path,
        kind: PhaseKind,
    ) -> Result<u64, PhaseInfraError> {
        use std::io::Write;

        let mut triple = open_session_phase_files(iter_dir, kind)?;

        // Flush the between-turn stderr buffer into the new turn's stderr file.
        {
            let mut buf_guard = self.between_turn_stderr.lock().await;
            if !buf_guard.bytes.is_empty() {
                let _ = triple.stderr.write_all(&buf_guard.bytes);
                buf_guard.bytes.clear();
                buf_guard.truncated = false;
            }
        }

        let mut guard = self.files.lock().await;
        *guard = Some(triple);
        let new_state = {
            let prev = self.turn.borrow();
            TurnState {
                kind,
                generation: prev.generation + 1,
            }
        };
        let gen = new_state.generation;
        let _ = self.turn.send(new_state);
        Ok(gen)
    }
```

Modify `shutdown` to flush remaining buffer into the last-active file (or discard if no file ever opened):

```rust
    // After the SIGTERM/SIGKILL block, before returning:
        let mut buf_guard = self.between_turn_stderr.lock().await;
        if !buf_guard.bytes.is_empty() {
            let mut files_guard = self.files.lock().await;
            if let Some(triple) = files_guard.as_mut() {
                use std::io::Write;
                let _ = triple.stderr.write_all(&buf_guard.bytes);
            }
            buf_guard.bytes.clear();
        }
```

(Add this block at the end of `shutdown`, just before the function returns.)

- [ ] **Step 3: Run the new test**

Run: `cargo test -p roki-daemon --features test-support --lib engine::session::tests::between_turn_stderr_flushes_into_next_turn`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/engine/session.rs
git commit -m "engine: buffer between-turn session stderr" -m "Drain task writes to the active turn's <phase>.stderr when files are open; otherwise into a 64 KiB RAM buffer that flushes on the next begin_turn or on shutdown. No new on-disk file — between-turn bytes always land in some <phase>.stderr per fr:04 capture contract." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 16: Extend `PhaseContext` with `run.terminal`

**Files:**
- Modify: `crates/roki-daemon/src/engine/context.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `crates/roki-daemon/src/engine/context.rs`:

```rust
    #[test]
    fn run_terminal_exposed_via_liquid() {
        let mut ctx = PhaseContext::new(/* slice-1 args */);
        let terminal = serde_json::json!({"is_error": false, "result": "ok"});
        ctx.set_run(0, 12, Some(terminal));
        let rendered = render_template(
            "{{ run.terminal.is_error }}/{{ run.terminal.result }}",
            &ctx,
        )
        .unwrap();
        assert_eq!(rendered, "false/ok");
    }

    #[test]
    fn run_terminal_clears_between_iters() {
        let mut ctx = PhaseContext::new(/* slice-1 args */);
        ctx.set_run(0, 1, Some(serde_json::json!({"is_error": false})));
        ctx.set_iter(2);
        let rendered = render_template("{{ run.terminal.is_error }}", &ctx).unwrap();
        assert_eq!(rendered, "");
    }
```

(Use the slice-1 helper for constructing a `PhaseContext`. If the helper takes specific args, copy them from any existing context test.)

- [ ] **Step 2: Run them and confirm they fail to compile**

Run: `cargo test -p roki-daemon --features test-support --lib engine::context::tests::run_terminal_exposed_via_liquid`
Expected: compile error — `set_run` signature doesn't match, `RunView::terminal` doesn't exist.

- [ ] **Step 3: Extend `RunView` and `set_run`**

In `crates/roki-daemon/src/engine/context.rs`, locate the `RunView` struct and extend it:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub exit_code: i32,
    pub duration_seconds: u64,
    /// `Some(value)` iff `iter-N/run.terminal.json` was written for the
    /// current iter (claude/codex `result` event surfaced).
    pub terminal: Option<serde_json::Value>,
}
```

Update `PhaseContext::set_run` (or whatever the slice-1 setter is called) to take a third argument:

```rust
    pub fn set_run(
        &mut self,
        exit_code: i32,
        duration_seconds: u64,
        terminal: Option<serde_json::Value>,
    ) {
        self.run = Some(RunView {
            exit_code,
            duration_seconds,
            terminal,
        });
    }
```

Update `set_iter` to clear `run`:

```rust
    pub fn set_iter(&mut self, iter: u32) {
        self.cycle.iter = iter;
        self.run = None;
        // pre / post cleared per the slice-1 contract — leave that block alone.
    }
```

(If slice-1 already cleared `run` here, leave it; the relevant change is that clearing now also clears `RunView::terminal`.)

- [ ] **Step 4: Update slice-1 callers of `set_run`**

Run `cargo build -p roki-daemon --tests` and add `None` as the third argument everywhere the compiler complains.

- [ ] **Step 5: Run the new tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::context`
Expected: every existing test plus the two new ones pass.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/engine/context.rs
git commit -m "engine: expose run.terminal in Liquid context" -m "RunView gains an optional terminal value populated from iter-N/run.terminal.json so post templates can branch on {{ run.terminal.is_error }} and friends. Cleared at set_iter to keep iters independent." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 17: Wire the supervisor into `engine::cycle::run_cycle`

**Files:**
- Modify: `crates/roki-daemon/src/engine/cycle.rs`
- Modify: `crates/roki-daemon/src/engine/phase.rs` (read `run.terminal.json` after run)

- [ ] **Step 1: Write a failing test for mixed-shape dispatch**

Append to `mod tests` in `crates/roki-daemon/src/engine/cycle.rs`:

```rust
    #[tokio::test]
    async fn mixed_shape_dispatches_through_supervisor_and_executor() {
        // pre = session, run = command, post = session — both pre and post
        // share a single child; run is its own process per iter.
        // ... build an ad-hoc fake supervisor + fake executor, drive run_cycle,
        // assert that the supervisor saw two run_turn calls (one pre + one post)
        // and the executor saw one run.
    }
```

(The detailed body depends on your slice-1 fake-executor harness. The acceptance check: `run_cycle` must call `SessionSupervisor::run_turn` for every session-shape phase invocation and `CommandPhaseExecutor::execute` for every command-shape phase invocation. Use trait/seam doubles; do not spawn real processes for this unit test.)

- [ ] **Step 2: Refactor `run_cycle` to dispatch by shape**

In `crates/roki-daemon/src/engine/cycle.rs`, replace the existing `run_cycle` body's "execute pre / run / post" block with shape-aware dispatch:

```rust
        // Build the supervisor lazily — only when at least one of pre/post
        // is session-shape. Run-phase session shape is rejected at config
        // load (Task 6), so run is always command.
        let needs_session = matches!(
            (rule.pre.as_ref(), rule.post.as_ref()),
            (Some(b), _) if b.shape() == PhaseShape::Session
        ) || matches!(
            rule.post.as_ref(),
            Some(b) if b.shape() == PhaseShape::Session
        );

        let supervisor = if needs_session {
            let session_cfg = build_session_config(cfg, &ctx, &cwd).await?;
            Some(SessionSupervisor::spawn(session_cfg).await?)
        } else {
            None
        };

        // Inside the iteration loop, replace `exec.execute(Pre, ...)` with:
        let pre_outcome = run_phase(
            &rule.pre,
            PhaseKind::Pre,
            &exec,
            supervisor.as_ref(),
            &ctx,
            &iter_dir,
        )
        .await?;
        // and similarly for Post.
```

Add a helper `run_phase` that switches on the body's shape:

```rust
async fn run_phase(
    body: &Option<crate::engine::outcome::PhaseBody>,
    kind: PhaseKind,
    exec: &CommandPhaseExecutor,
    supervisor: Option<&SessionSupervisor>,
    ctx: &PhaseContext,
    iter_dir: &std::path::Path,
) -> Result<crate::engine::outcome::PhaseOutcome, PhaseInfraError> {
    let Some(body) = body else {
        // Synthesised "run" / "end" — same as slice 1.
        return Ok(synthesised_outcome(kind));
    };
    match body.shape() {
        PhaseShape::Command => exec.execute(kind, body, ctx, iter_dir).await,
        PhaseShape::Session => {
            let sup = supervisor.expect("session phase but supervisor not constructed");
            // Render body to bytes (Liquid render — slice-1 helper).
            let rendered = crate::engine::template::render_phase_body(body, ctx)?;
            sup.run_turn(iter_dir, kind, rendered.stdin_body.as_bytes(), body.stall_seconds_override())
                .await
        }
    }
}
```

Add `build_session_config` near the bottom of `cycle.rs`:

```rust
async fn build_session_config(
    cfg: &crate::config::roki::RokiConfig,
    ctx: &PhaseContext,
    cwd: &std::path::Path,
) -> Result<SessionConfig, PhaseInfraError> {
    let session_cfg = cfg
        .default_ai_session
        .as_ref()
        .ok_or(PhaseInfraError::SessionCliMissing)?;
    let cli_template = session_cfg
        .cli
        .as_deref()
        .ok_or(PhaseInfraError::SessionCliMissing)?;
    let rendered = crate::engine::template::render_str(cli_template, ctx)
        .map_err(|_| PhaseInfraError::SessionCliMissing)?;
    let argv = shell_words::split(&rendered).map_err(|_| PhaseInfraError::SessionCliMissing)?;
    Ok(SessionConfig {
        cli: rendered,
        argv,
        default_stall_seconds: session_cfg.stall_seconds,
        cwd: cwd.to_path_buf(),
        envs: ctx.roki_env_pairs().chain(passthrough_env()).collect(),
    })
}

fn passthrough_env() -> impl Iterator<Item = (String, String)> {
    ["PATH", "HOME", "USER"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
}
```

(`render_phase_body` and `render_str` are slice-1 helpers; if their actual names differ, use whatever slice 1 exposes — visible in `crates/roki-daemon/src/engine/template.rs`.)

- [ ] **Step 3: After every Run-phase command outcome, read `run.terminal.json` if present**

Find the `RunDone { exit_code, duration_seconds }` case in `run_cycle`. Replace `ctx.set_run(exit_code, duration_seconds)` with:

```rust
                let terminal = std::fs::read_to_string(iter_dir.join("run.terminal.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                ctx.set_run(exit_code, duration_seconds, terminal);
```

- [ ] **Step 4: Call `supervisor.shutdown(...)` on every exit path**

At the end of `run_cycle`, just before each `return`, add:

```rust
        if let Some(sup) = supervisor.as_ref() {
            sup.shutdown(reason).await;
        }
```

with `reason` resolved from the local outcome (`Completed` for terminal directive, `IterExhausted` for iter cap, `Failed` for any other failure). Use a `let reason = ...;` block before the return to keep each path explicit.

- [ ] **Step 5: Run the cycle tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::cycle`
Expected: every existing slice-1 test plus the new mixed-shape test pass.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/engine/cycle.rs crates/roki-daemon/src/engine/phase.rs
git commit -m "engine: cycle dispatches per-phase by shape" -m "run_cycle constructs SessionSupervisor lazily when any pre/post is session-shape, dispatches each phase to executor or supervisor by shape, and reads run.terminal.json into PhaseContext.run.terminal between iterations." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 18: Reject run-shape-Session at workflow load and add `SessionRunUnsupported` enforcement

**Files:**
- Modify: `crates/roki-daemon/src/config/workflow.rs`

- [ ] **Step 1: Write a failing test**

Append to `mod tests` in `crates/roki-daemon/src/config/workflow.rs`:

```rust
    #[test]
    fn run_phase_session_shape_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let workflow_md = dir.path().join("foo.md");
        std::fs::write(
            &workflow_md,
            "---\nsession: \"session\"\n---\nbody\n",
        )
        .unwrap();
        let workflow_toml = dir.path().join("WORKFLOW.toml");
        std::fs::write(
            &workflow_toml,
            r#"
[[rule]]
when.label = "x"

[rule.run]
path = "foo.md"
"#,
        )
        .unwrap();
        match WorkflowConfig::load(&workflow_toml) {
            Err(WorkflowError::SessionRunUnsupported { .. }) => {}
            other => panic!("expected SessionRunUnsupported, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run it and confirm it fails**

Run: `cargo test -p roki-daemon --features test-support --lib config::workflow::tests::run_phase_session_shape_is_rejected`
Expected: failure — current loader accepts session shape on run.

- [ ] **Step 3: Add the rejection**

In `crates/roki-daemon/src/config/workflow.rs`, find the function that materialises a `Rule` from the parsed TOML (around `parse_rule_block` or wherever slice-1 builds the `Rule { pre, run, post }` literal). After the `run` body is constructed, add:

```rust
        if run_body.shape() == crate::engine::outcome::PhaseShape::Session {
            return Err(WorkflowError::SessionRunUnsupported {
                path: path.to_path_buf(),
            });
        }
```

(`run_body` is the local variable name slice 1 used; substitute the actual name.)

- [ ] **Step 4: Run the workflow tests**

Run: `cargo test -p roki-daemon --features test-support --lib config::workflow`
Expected: every existing test plus the new one pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/config/workflow.rs
git commit -m "config: reject run-phase session shape" -m "Slice 2 deliberately narrows fr:04 mix-and-match by rejecting [[rule.run]] resolved to PhaseShape::Session at config load. The narrowing is a scope deferral, not an FR contract change; a later slice lifts it." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 19: End-to-end smoke — `session_smoke`

**Files:**
- Create: `crates/roki-daemon/tests/e2e/session_smoke.rs`
- Create: `crates/roki-daemon/tests/e2e/fixtures/fake_session_agent.sh`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add the test target**

Edit `crates/roki-daemon/Cargo.toml`. Inside the existing `[[test]]` blocks (or add at the end of `[dependencies]`-area), append:

```toml
[[test]]
name = "session_smoke"
path = "tests/e2e/session_smoke.rs"
```

- [ ] **Step 2: Create the fake agent fixture**

Create `crates/roki-daemon/tests/e2e/fixtures/fake_session_agent.sh` (executable):

```bash
#!/usr/bin/env bash
set -euo pipefail
# Side-channel: write our PID once to the path in $ROKI_TEST_PID_FILE so the
# test harness can verify the same child handled both turns.
if [ -n "${ROKI_TEST_PID_FILE:-}" ] && [ ! -e "$ROKI_TEST_PID_FILE" ]; then
  printf '%s\n' "$$" > "$ROKI_TEST_PID_FILE"
fi
counter_file="${ROKI_TEST_COUNTER_FILE:-/tmp/roki-fake-session-counter}"
while IFS= read -r _line; do
  count=$(cat "$counter_file" 2>/dev/null || echo 0)
  count=$((count + 1))
  printf '%s' "$count" > "$counter_file"
  if [ "$count" -lt 2 ]; then
    printf '{"directive":"run","note":"continue"}\n'
  else
    printf '{"directive":"end","note":"done"}\n'
  fi
done
```

After creating the file, run `chmod +x crates/roki-daemon/tests/e2e/fixtures/fake_session_agent.sh`.

- [ ] **Step 3: Write the smoke test**

Create `crates/roki-daemon/tests/e2e/session_smoke.rs`:

```rust
//! End-to-end smoke: a 2-iter cycle where pre and post are session-shape
//! `prompt` bodies. The fake agent is reused across both iterations; the
//! test asserts the on-disk layout and that the same child PID handled both
//! turns.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;

mod common;
use common::{post_webhook, start_daemon, wait_for_exit};

#[test]
fn session_two_iter_smoke() {
    let tmp = TempDir::new().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();
    let counter = tmp.path().join("counter");
    let pid_file = tmp.path().join("agent.pid");
    let workflow_dir = tmp.path().join("workflow");
    std::fs::create_dir_all(&workflow_dir).unwrap();

    let agent = workflow_dir.join("agent.sh");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/e2e/fixtures/fake_session_agent.sh"),
        &agent,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&agent).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&agent, perms).unwrap();

    let workflow_toml = tmp.path().join("WORKFLOW.toml");
    std::fs::write(
        &workflow_toml,
        format!(
            r#"
[[rule]]
when.label = "ai"

[rule.pre]
prompt = "tick"

[rule.run]
cmd = "true"

[rule.post]
prompt = "tock"
"#
        ),
    )
    .unwrap();

    let roki_toml = tmp.path().join("roki.toml");
    std::fs::write(
        &roki_toml,
        format!(
            r#"
[linear]
token = "t"

[linear.webhook]
bind = "127.0.0.1"
port = 0

[default.ai.command]
cli = "true"

[default.ai.session]
cli = "{}"
stall_seconds = 5

[engine]
max_iterations = 2

[paths]
workflow = "{}"
session_root = "{}"
"#,
            agent.display(),
            workflow_toml.display(),
            session_root.display()
        ),
    )
    .unwrap();

    let mut daemon = start_daemon(&roki_toml)
        .env("ROKI_TEST_COUNTER_FILE", &counter)
        .env("ROKI_TEST_PID_FILE", &pid_file)
        .spawn()
        .unwrap();

    post_webhook(&daemon, "ENG-1", "ai");
    let exit = wait_for_exit(daemon, Duration::from_secs(15));
    assert!(exit.success(), "daemon exit failure");

    // Layout
    let cycle_root = session_root.join("ENG-1");
    let cycle_dir = std::fs::read_dir(&cycle_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    for iter in [1, 2] {
        let iter_dir = cycle_dir.join(format!("iter-{iter}"));
        for f in [
            "pre.stdout",
            "pre.stderr",
            "pre.events.jsonl",
            "pre.response.json",
            "run.exit_code",
            "post.stdout",
            "post.stderr",
            "post.events.jsonl",
            "post.response.json",
        ] {
            assert!(
                iter_dir.join(f).is_file(),
                "missing {f} in {}",
                iter_dir.display()
            );
        }
        assert!(
            !iter_dir.join("run.terminal.json").exists(),
            "run is plain shell — terminal must be absent"
        );
    }

    // Same agent PID across iterations.
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    assert!(!pid.trim().is_empty(), "pid file should be populated once");
}
```

`common` should be your slice-1 helper module (`tests/e2e/common.rs`). If slice-1 already provides `start_daemon` / `post_webhook` / `wait_for_exit`, reuse them; otherwise replicate from `iteration_smoke.rs`.

- [ ] **Step 4: Run the smoke test**

Run: `cargo test -p roki-daemon --features test-support --test session_smoke`
Expected: pass within ~15 s.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/tests/e2e/session_smoke.rs crates/roki-daemon/tests/e2e/fixtures/fake_session_agent.sh
git commit -m "test(e2e): session two-iter smoke" -m "Drives the daemon end-to-end with prompt-form pre/post phases. Asserts the per-iter capture layout and that the fake agent was a single child reused across both turns." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 20: End-to-end smoke — `stall_smoke`

**Files:**
- Create: `crates/roki-daemon/tests/e2e/stall_smoke.rs`
- Create: `crates/roki-daemon/tests/e2e/fixtures/sleep_run.sh`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add the test target**

```toml
[[test]]
name = "stall_smoke"
path = "tests/e2e/stall_smoke.rs"
```

- [ ] **Step 2: Create the fixture**

Create `crates/roki-daemon/tests/e2e/fixtures/sleep_run.sh` with:

```bash
#!/usr/bin/env bash
# Block both SIGTERM (so the watchdog must escalate to SIGKILL) and stdout
# emission, so the stall window expires and the supervisor must terminate.
trap '' TERM
sleep 30
```

`chmod +x` the file.

- [ ] **Step 3: Write the smoke test**

Create `crates/roki-daemon/tests/e2e/stall_smoke.rs`. Mirror `session_smoke.rs` shape; the rule has `[rule.run] cmd = "<absolute path to sleep_run.sh>"` and `[default.ai.command].stall_seconds = 1`. Assert:

- daemon exits non-zero
- `iter-1/run.stdout` exists (capture preserved)
- `iter-1/run.terminal.json` does not exist
- elapsed wall clock < 15 s (proves SIGKILL escalation finished within `1 s + 5 s grace`)

```rust
//! End-to-end smoke: a run phase that ignores SIGTERM and emits no stdout
//! triggers stall detection; the daemon must exit 1 within the
//! stall window + grace period.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

mod common;
use common::{post_webhook, start_daemon, wait_for_exit};

#[test]
fn run_phase_stall_terminates_within_grace() {
    let tmp = TempDir::new().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let sleep_script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/e2e/fixtures/sleep_run.sh");

    let workflow_toml = tmp.path().join("WORKFLOW.toml");
    std::fs::write(
        &workflow_toml,
        format!(
            r#"
[[rule]]
when.label = "stall"

[rule.run]
cmd = "{}"
"#,
            sleep_script.display()
        ),
    )
    .unwrap();

    let roki_toml = tmp.path().join("roki.toml");
    std::fs::write(
        &roki_toml,
        format!(
            r#"
[linear]
token = "t"

[linear.webhook]
bind = "127.0.0.1"
port = 0

[default.ai.command]
cli = "true"
stall_seconds = 1

[engine]
max_iterations = 1

[paths]
workflow = "{}"
session_root = "{}"
"#,
            workflow_toml.display(),
            session_root.display()
        ),
    )
    .unwrap();

    let started = Instant::now();
    let mut daemon = start_daemon(&roki_toml).spawn().unwrap();
    post_webhook(&daemon, "ENG-2", "stall");
    let exit = wait_for_exit(daemon, Duration::from_secs(15));
    assert!(!exit.success());
    assert!(started.elapsed() < Duration::from_secs(15));

    let cycle_root = session_root.join("ENG-2");
    let cycle_dir = std::fs::read_dir(&cycle_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let iter_dir = cycle_dir.join("iter-1");
    assert!(iter_dir.join("run.stdout").is_file());
    assert!(!iter_dir.join("run.terminal.json").exists());
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p roki-daemon --features test-support --test stall_smoke`
Expected: pass within ~12 s.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/tests/e2e/stall_smoke.rs crates/roki-daemon/tests/e2e/fixtures/sleep_run.sh
git commit -m "test(e2e): stall + sigkill escalation smoke" -m "Run phase ignores SIGTERM and emits no stdout. Daemon must exit 1 within stall_seconds + grace; capture preserved on disk." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 21: End-to-end smoke — `run_terminal_smoke`

**Files:**
- Create: `crates/roki-daemon/tests/e2e/run_terminal_smoke.rs`
- Create: `crates/roki-daemon/tests/e2e/fixtures/fake_run_terminal.sh`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add the test target**

```toml
[[test]]
name = "run_terminal_smoke"
path = "tests/e2e/run_terminal_smoke.rs"
```

- [ ] **Step 2: Create the fixture**

Create `crates/roki-daemon/tests/e2e/fixtures/fake_run_terminal.sh`:

```bash
#!/usr/bin/env bash
printf '%s\n' '{"type":"thinking","text":"working"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"ok"}'
```

`chmod +x` the file.

- [ ] **Step 3: Write the smoke test**

Create `crates/roki-daemon/tests/e2e/run_terminal_smoke.rs`. The rule has:
- `[rule.run] cmd = "<absolute path to fake_run_terminal.sh>"`
- `[rule.post] cmd = "echo terminal_is_error={{ run.terminal.is_error }} 1>&2; printf '{\"directive\":\"end\"}\n'"`

Assertions:
- daemon exits 0
- `iter-1/run.terminal.json` exists and contains `is_error`
- `iter-1/post.stderr` contains `terminal_is_error=false` (proves the Liquid context was populated)

(Mirror `session_smoke.rs` for the harness wiring; the rule body is the only difference.)

- [ ] **Step 4: Run the test**

Run: `cargo test -p roki-daemon --features test-support --test run_terminal_smoke`
Expected: pass within ~10 s.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/tests/e2e/run_terminal_smoke.rs crates/roki-daemon/tests/e2e/fixtures/fake_run_terminal.sh
git commit -m "test(e2e): run.terminal.json round trip" -m "Run phase emits a stream-json result event; the post template reads {{ run.terminal.is_error }} via the Liquid context. Verifies mid-stream extraction and PhaseContext round-trip end-to-end." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Final verification

- [ ] **Run the full crate test suite**

Run: `cargo test -p roki-daemon --features test-support`
Expected: every unit + integration + e2e test passes.

- [ ] **Run clippy**

Run: `cargo clippy -p roki-daemon --features test-support --all-targets -- -D warnings`
Expected: clean.

- [ ] **Run doctools validate**

Run: `kusara validate`
Expected: `OK (N docs)` with no failures. The slice-2 design and plan files live under `docs/superpowers/` — outside the graph by design.

- [ ] **Open the slice-2 PR**

Branch `slice2-session-streamjson` → `main`. Title: `slice 2: session-shape and stream-json`. Body summarises:

- session-shape pre/post via `SessionSupervisor`,
- stream-json line-by-line capture (`events.jsonl`),
- stall detection (SIGTERM + 5 s grace + SIGKILL),
- per-file `stall_seconds` + `session` overrides via workflow .md frontmatter,
- `run.terminal.json` mid-stream extraction,
- `{{ run.terminal.* }}` in the Liquid post context,
- run-phase session shape rejected at config load (slice-2 scope deferral),
- 5 fr/04 drifts resolved per the spec drift-fix commit.
