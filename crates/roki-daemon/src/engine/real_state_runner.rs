//! Production state runner.
//!
//! Spawns one subprocess per state visit, drives the watchdog, scans stdout
//! for the claude/codex stream-json `result` event, reads the sentinel file
//! at exit, and translates the result into a `StateOutcome` for
//! `engine::cycle_state::run_cycle`.
//!
//! Spec: §2.4, §5, §6. fr:04 §Capture, §Sentinel directive contract.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{Map, Value};
use shell_words;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use uuid::Uuid;

use crate::capture;
use crate::daemon::inflight::{Inflight, InflightRegistry};
use crate::daemon::shutdown::ShutdownToken;
use crate::engine::cwd;
use crate::engine::sentinel::{self, DirectivePayload, SentinelError};
use crate::engine::stall::{StallOutcome, Watchdog};
use crate::engine::stream::{LineSplitter, scan_run_terminal_line};
use crate::engine::template;
use crate::workflow::canonical::{State, StateBody, resolve_directive};

use super::outcome::FailureKind;
use super::state_runtime::{CycleContext, StateOutcome, StateRunner, TaskCaptures};

/// Per-cycle state runner. Constructed by `daemon::real_runner` at cycle
/// start; consumed by `engine::cycle_state::run_cycle` for every state visit.
pub struct RealStateRunner {
    /// `[default.ai].cli` from `roki.toml`. Argv source for `uses:` states
    /// that don't carry a `cli:` frontmatter override (Slice 8 collapses
    /// `[default.ai.command]` and `[default.ai.session]` into `[default.ai]`).
    pub default_cli: String,
    /// Stall window applied when neither the cli line nor a state-level
    /// `timeout:` overrides it.
    pub default_stall_seconds: u32,
    /// Linear ticket id; used to resolve the worktree / ghq base via
    /// `engine::cwd::resolve` and to seed `<session_root>/<ticket>/...` paths.
    pub ticket_id: String,
    /// `repo.ghq` from the admitted ticket. Passed to `cwd::resolve`.
    pub ghq: String,
    /// Top-level capture dir for this cycle (`<session_root>`).
    pub session_root: PathBuf,
    /// `<session_root>/<ticket>/`. Sentinel files land in
    /// `<session_tempdir>/directives/<state_id>.<visit_n>.json`.
    pub session_tempdir: PathBuf,
    /// Stable cycle UUID; appears in capture paths as `cycle-<uuid>/`.
    pub cycle_id: Uuid,
    /// Fires on SIGINT / SIGTERM. The runner SIGTERMs the live child when
    /// this becomes ready and reaps normally afterward.
    pub shutdown: ShutdownToken,
    /// Process-wide live-subprocess registry. The runner registers right
    /// after spawn and clears right after reap.
    pub inflight: Arc<InflightRegistry>,
}

#[async_trait]
impl StateRunner for RealStateRunner {
    async fn run_state(&self, state: &State, ctx: &CycleContext) -> StateOutcome {
        // The cycle driver bumped `visits[state.id]` before calling us; that
        // value is the current visit_n.
        let visit_n = ctx.visits.get(&state.id).copied().unwrap_or(1);

        // 1. Build per-state Liquid globals: cycle, ticket, repo, config from
        //    `ctx.globals`; state.* and tasks.* derived per-call.
        let liquid_globals = build_liquid_globals(state, ctx, visit_n);

        // 2. Allocate per-visit capture dir.
        let visit_dir = match capture::create_visit_dir(
            &self.session_root,
            &self.ticket_id,
            self.cycle_id,
            ctx.iter,
        ) {
            Ok(p) => p,
            Err(err) => {
                return StateOutcome::Failure {
                    kind: FailureKind::FsPoison,
                    error_text: format!("visit-dir create: {err}"),
                };
            }
        };

        // 3. Allocate the sentinel path.
        let sentinel_path = match sentinel::allocate_path(&self.session_tempdir, &state.id, visit_n)
        {
            Ok(p) => p,
            Err(err) => {
                return StateOutcome::Failure {
                    kind: FailureKind::FsPoison,
                    error_text: format!("sentinel path: {err}"),
                };
            }
        };

        // 4. Resolve cli + stdin templates from the state body.
        let (argv_template, stdin_template) = match &state.body {
            StateBody::Run { cmd } => (format!("sh -c {}", shell_words::quote(cmd)), None),
            StateBody::Uses { path } => match read_uses_body(path).await {
                Ok((cli, body)) => (cli.unwrap_or_else(|| self.default_cli.clone()), Some(body)),
                Err(err) => {
                    return StateOutcome::Failure {
                        kind: FailureKind::FsPoison,
                        error_text: format!("uses '{}': {err}", path.display()),
                    };
                }
            },
        };

        // 5. Liquid-render argv + stdin.
        let argv_rendered = match template::render_str_with_globals(&argv_template, &liquid_globals)
        {
            Ok(s) => s,
            Err(err) => {
                return StateOutcome::Failure {
                    kind: FailureKind::TemplateError,
                    error_text: format!("argv: {err}"),
                };
            }
        };
        let stdin_rendered = match stdin_template {
            Some(t) => match template::render_str_with_globals(&t, &liquid_globals) {
                Ok(s) => Some(s),
                Err(err) => {
                    return StateOutcome::Failure {
                        kind: FailureKind::TemplateError,
                        error_text: format!("stdin: {err}"),
                    };
                }
            },
            None => None,
        };

        // 6. shell-words split argv.
        let argv = match shell_words::split(&argv_rendered) {
            Ok(v) => v,
            Err(err) => {
                return StateOutcome::Failure {
                    kind: FailureKind::TemplateError,
                    error_text: format!("argv split: {err}"),
                };
            }
        };
        let Some((bin, rest)) = argv.split_first() else {
            return StateOutcome::Failure {
                kind: FailureKind::TemplateError,
                error_text: "argv is empty after rendering".into(),
            };
        };

        // 7. Resolve cwd (worktree if present, else ghq base).
        let cwd_path = match cwd::resolve(&self.ghq, &self.ticket_id).await {
            Ok(p) => p,
            Err(err) => {
                return StateOutcome::Failure {
                    kind: FailureKind::FsPoison,
                    error_text: format!("cwd resolve: {err}"),
                };
            }
        };

        // 8. Open capture files.
        let (stdout_file, stderr_file) = match capture::open_state_files(&visit_dir, &state.id) {
            Ok(p) => p,
            Err(err) => {
                return StateOutcome::Failure {
                    kind: FailureKind::FsPoison,
                    error_text: format!("capture open: {err}"),
                };
            }
        };

        // 9. Build env.
        let env_pairs = build_env(state, ctx, visit_n, &sentinel_path);

        // 10. Resolve stall window: per-state `timeout:` overrides default.
        let stall_seconds = state
            .timeout
            .map(|d| d.as_secs() as u32)
            .unwrap_or(self.default_stall_seconds);

        // 11. Spawn.
        let mut cmd = Command::new(bin);
        cmd.args(rest)
            .current_dir(&cwd_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin_rendered.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.env_clear();
        for var in ["PATH", "HOME", "USER"] {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }
        for (k, v) in env_pairs {
            cmd.env(k, v);
        }

        let started = Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                return StateOutcome::Failure {
                    kind: FailureKind::ProcessCrash,
                    error_text: format!("spawn '{}': {err}", argv_rendered),
                };
            }
        };

        let pid = child.id().unwrap_or(0);
        self.inflight
            .register(Inflight {
                ticket_id: self.ticket_id.clone(),
                cycle_id: self.cycle_id,
                state_id: state.id.clone(),
                visit: visit_n,
                pid,
            })
            .await;

        // 12. Write stdin once if needed.
        if let Some(body) = stdin_rendered.as_ref() {
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(err) = stdin.write_all(body.as_bytes()).await {
                    self.inflight.clear(&self.ticket_id).await;
                    return StateOutcome::Failure {
                        kind: FailureKind::ProcessCrash,
                        error_text: format!("stdin write: {err}"),
                    };
                }
                drop(stdin);
            }
        }

        // 13. Drain stdout (tee to file + scan for terminal) and stderr.
        let stdout_pipe = child.stdout.take().expect("piped");
        let stderr_pipe = child.stderr.take().expect("piped");
        let watchdog = Watchdog::new(stall_seconds);

        let visit_dir_for_terminal = visit_dir.clone();
        let state_id_for_terminal = state.id.clone();
        let stdout_task = {
            let wd = watchdog.clone();
            tokio::spawn(async move {
                tee_stdout(
                    stdout_pipe,
                    stdout_file,
                    wd,
                    visit_dir_for_terminal,
                    state_id_for_terminal,
                )
                .await
            })
        };
        let stderr_task = tokio::spawn(async move {
            drain_stderr(stderr_pipe, stderr_file).await;
        });

        // 14. Watchdog runs in parallel with a shutdown observer. On shutdown
        // fire we SIGTERM the live child (reusing `engine::stall::
        // terminate_child_external`, which does TERM → 5 s grace → KILL
        // → reap). The watchdog's `Healthy` short-circuit covers the race
        // where the child exits cleanly before SIGTERM lands.
        let stall_outcome = {
            let shutdown = self.shutdown.clone();
            tokio::select! {
                biased;
                outcome = watchdog.run(&mut child) => outcome,
                _ = shutdown.wait() => {
                    crate::engine::stall::terminate_child_external(&mut child).await;
                    // Treat as "Healthy" for the wait-and-reap path below;
                    // the resulting exit_status is a signal kill, which
                    // step 17 already classifies as `ProcessCrash`.
                    StallOutcome::Healthy
                }
            }
        };
        let terminal_payload = stdout_task.await.unwrap_or(None);
        let _ = stderr_task.await;

        // 15. Reap.
        let exit_status = match child.wait().await {
            Ok(s) => s,
            Err(err) => {
                self.inflight.clear(&self.ticket_id).await;
                return StateOutcome::Failure {
                    kind: FailureKind::ProcessCrash,
                    error_text: format!("wait: {err}"),
                };
            }
        };
        self.inflight.clear(&self.ticket_id).await;
        let duration_seconds = started.elapsed().as_secs();

        // 16. Stall trumps everything else.
        if stall_outcome == StallOutcome::StalledThenTerminated {
            return StateOutcome::Failure {
                kind: FailureKind::Stall,
                error_text: format!("stdout silent for {stall_seconds}s; SIGTERM sent"),
            };
        }

        // 17. Process killed by signal without sentinel write → ProcessCrash.
        let exit_code = match exit_status.code() {
            Some(c) => c,
            None => {
                return StateOutcome::Failure {
                    kind: FailureKind::ProcessCrash,
                    error_text: "subprocess terminated by signal".into(),
                };
            }
        };

        // 18. Persist exit_code.
        let _ = capture::write_state_exit_code(&visit_dir, &state.id, exit_code);

        // 19. Read sentinel.
        let directive = match sentinel::read_sentinel(&sentinel_path) {
            Ok(opt) => opt,
            Err(SentinelError::Unparseable { detail, .. }) => {
                return StateOutcome::Failure {
                    kind: FailureKind::Unparseable,
                    error_text: format!("sentinel unparseable: {detail}"),
                };
            }
            Err(err) => {
                return StateOutcome::Failure {
                    kind: FailureKind::FsPoison,
                    error_text: format!("sentinel read: {err}"),
                };
            }
        };

        // 20. Resolve next edge.
        let next = match (&directive, exit_code) {
            (None, 0) => state.on_done.clone(),
            (None, _) => state.on_fail.clone(),
            (Some(payload), _) => match resolve_directive(&payload.directive, state) {
                Some(target) => {
                    let payload_value = directive_to_value(payload);
                    let _ =
                        capture::write_state_directive_json(&visit_dir, &state.id, &payload_value);
                    target
                }
                None => {
                    return StateOutcome::Failure {
                        kind: FailureKind::SchemaDrift,
                        error_text: format!(
                            "directive '{}' not in state.directives ∪ defaults",
                            payload.directive
                        ),
                    };
                }
            },
        };

        StateOutcome::Edge {
            next,
            captures: TaskCaptures {
                exit_code,
                duration_seconds,
                directive,
                terminal: terminal_payload,
            },
        }
    }
}

/// Read a `uses:` workflow file: strip optional YAML frontmatter, return the
/// body and any `cli:` override declared in frontmatter. Slice 8 frontmatter
/// schema is documented in `docs/reference/frontmatter.md`.
async fn read_uses_body(path: &Path) -> std::io::Result<(Option<String>, String)> {
    let raw = tokio::fs::read_to_string(path).await?;
    let (frontmatter, body) = split_frontmatter(&raw);
    let cli_override = frontmatter.and_then(extract_cli_field);
    Ok((cli_override, body.to_string()))
}

/// Split optional YAML frontmatter (`---` … `---` at file start) from the
/// body. Returns `(Some(frontmatter), body)` when delimited, `(None, raw)`
/// otherwise.
fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let trimmed = raw.trim_start();
    let leading = raw.len() - trimmed.len();
    if !trimmed.starts_with("---") {
        return (None, raw);
    }
    let after_open = match trimmed.strip_prefix("---") {
        Some(rest) => rest.trim_start_matches('\n'),
        None => return (None, raw),
    };
    if let Some(close_idx) = after_open.find("\n---") {
        let frontmatter = &after_open[..close_idx];
        let after_close = &after_open[close_idx + 4..];
        let body = after_close.trim_start_matches('\n');
        // Recover the body slice from the original `raw` so the lifetimes line up.
        let body_start = raw.len() - body.len();
        let body_slice = &raw[body_start..];
        let _ = leading; // currently unused; reserved for future error contexts
        return (Some(frontmatter), body_slice);
    }
    (None, raw)
}

/// Pull a top-level scalar `cli:` field from a YAML frontmatter snippet
/// without dragging in `serde_yaml`. The frontmatter is small and the schema
/// is fixed, so a line-prefix scan suffices.
fn extract_cli_field(frontmatter: &str) -> Option<String> {
    for line in frontmatter.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("cli:") {
            let value = rest.trim();
            if value.is_empty() {
                return None;
            }
            // Strip surrounding single or double quotes, if any.
            let unquoted = value.trim_matches(|c| c == '"' || c == '\'');
            return Some(unquoted.to_string());
        }
    }
    None
}

/// Build the per-state Liquid globals object: cycle/ticket/repo/config from
/// `ctx.globals`; state.id, state.visit_n, and tasks.<id>.* derived per-call.
fn build_liquid_globals(state: &State, ctx: &CycleContext, visit_n: u32) -> liquid::Object {
    let mut globals = serde_object_to_liquid(&ctx.globals);

    // Set/overwrite cycle.iter to reflect the current visit count.
    if let Some(liquid::model::Value::Object(cycle)) = globals.get_mut("cycle") {
        cycle.insert("iter".into(), liquid::model::Value::scalar(ctx.iter as i64));
    }

    // state.* (current invocation).
    let mut state_obj = liquid::Object::new();
    state_obj.insert("id".into(), liquid::model::Value::scalar(state.id.clone()));
    state_obj.insert(
        "visit_n".into(),
        liquid::model::Value::scalar(visit_n as i64),
    );
    globals.insert("state".into(), liquid::model::Value::Object(state_obj));

    // tasks.<state_id>.{exit_code, duration_seconds, directive, terminal}
    let mut tasks_obj = liquid::Object::new();
    for (id, captures) in &ctx.task_captures {
        tasks_obj.insert(
            id.clone().into(),
            liquid::model::Value::Object(task_captures_to_object(captures)),
        );
    }
    globals.insert("tasks".into(), liquid::model::Value::Object(tasks_obj));

    globals
}

fn task_captures_to_object(c: &TaskCaptures) -> liquid::Object {
    let mut obj = liquid::Object::new();
    obj.insert(
        "exit_code".into(),
        liquid::model::Value::scalar(c.exit_code as i64),
    );
    obj.insert(
        "duration_seconds".into(),
        liquid::model::Value::scalar(c.duration_seconds as i64),
    );
    if let Some(directive) = &c.directive {
        obj.insert(
            "directive".into(),
            liquid::model::Value::Object(directive_to_liquid_object(directive)),
        );
    }
    if let Some(terminal) = &c.terminal {
        obj.insert(
            "terminal".into(),
            liquid::model::to_value(terminal).unwrap_or(liquid::model::Value::Nil),
        );
    }
    obj
}

fn directive_to_liquid_object(d: &DirectivePayload) -> liquid::Object {
    let mut obj = liquid::Object::new();
    obj.insert(
        "directive".into(),
        liquid::model::Value::scalar(d.directive.clone()),
    );
    if let Some(outcome) = &d.outcome {
        obj.insert(
            "outcome".into(),
            liquid::model::Value::scalar(outcome.clone()),
        );
    }
    for (k, v) in &d.extra {
        obj.insert(
            k.clone().into(),
            liquid::model::to_value(v).unwrap_or(liquid::model::Value::Nil),
        );
    }
    obj
}

fn directive_to_value(d: &DirectivePayload) -> Value {
    let mut map = Map::new();
    map.insert("directive".into(), Value::String(d.directive.clone()));
    if let Some(outcome) = &d.outcome {
        map.insert("outcome".into(), Value::String(outcome.clone()));
    }
    for (k, v) in &d.extra {
        map.insert(k.clone(), v.clone());
    }
    Value::Object(map)
}

/// Convert a `serde_json::Map` to a `liquid::Object` by round-tripping each
/// value through `liquid::model::to_value`.
fn serde_object_to_liquid(map: &Map<String, Value>) -> liquid::Object {
    map.iter()
        .map(|(k, v)| {
            (
                k.clone().into(),
                liquid::model::to_value(v).unwrap_or(liquid::model::Value::Nil),
            )
        })
        .collect()
}

/// Build the `ROKI_*` env pairs for one state subprocess. Always exports
/// `ROKI_DIRECTIVE_PATH`. Top-level scalars from `ctx.globals.ticket`,
/// `repo`, `cycle`, and `config` are flattened into `ROKI_<NAMESPACE>_<KEY>`.
/// `ROKI_STATE_ID` and `ROKI_STATE_VISITS` reflect the current invocation.
/// Past task captures are flattened into
/// `ROKI_TASK_<STATE_ID>_{EXIT_CODE,DURATION_SECONDS,...}` per spec §6.
fn build_env(
    state: &State,
    ctx: &CycleContext,
    visit_n: u32,
    sentinel_path: &Path,
) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    pairs.push((
        "ROKI_DIRECTIVE_PATH".into(),
        sentinel_path.to_string_lossy().into_owned(),
    ));
    pairs.push(("ROKI_STATE_ID".into(), state.id.clone()));
    pairs.push(("ROKI_STATE_VISITS".into(), visit_n.to_string()));

    for (ns_key, ns_value) in &ctx.globals {
        if let Value::Object(map) = ns_value {
            push_namespace_scalars(&mut pairs, ns_key, map);
        }
    }
    // cycle.iter is mutated per visit; ensure the env reflects the current
    // count even if `globals.cycle.iter` is stale from cycle bootstrap.
    pairs.push(("ROKI_CYCLE_ITER".into(), ctx.iter.to_string()));

    for (state_id, captures) in &ctx.task_captures {
        let prefix = state_id_to_env(state_id);
        for (suffix, value) in captures.env_scalars() {
            pairs.push((format!("ROKI_TASK_{prefix}_{suffix}"), value));
        }
        for (suffix, value) in captures.directive_extra_env() {
            pairs.push((format!("ROKI_TASK_{prefix}_DIRECTIVE_{suffix}"), value));
        }
    }
    pairs
}

fn push_namespace_scalars(
    pairs: &mut Vec<(String, String)>,
    namespace: &str,
    map: &Map<String, Value>,
) {
    for (key, value) in map {
        let scalar = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        let upper_ns = namespace.to_ascii_uppercase();
        let upper_key = key.to_ascii_uppercase();
        if !is_legal_env_ident(&upper_ns) || !is_legal_env_ident(&upper_key) {
            continue;
        }
        pairs.push((format!("ROKI_{upper_ns}_{upper_key}"), scalar));
    }
}

fn is_legal_env_ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| matches!(b, b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

/// Sanitise a state id for env var use: uppercase ASCII alnum + `_`, others
/// dropped. Empty result falls back to `_` so the env var name is still valid.
fn state_id_to_env(state_id: &str) -> String {
    let mut out: String = state_id
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

async fn tee_stdout(
    pipe: tokio::process::ChildStdout,
    mut file: std::fs::File,
    watchdog: Watchdog,
    visit_dir: PathBuf,
    state_id: String,
) -> Option<Value> {
    use std::io::Write as _;
    let mut splitter = LineSplitter::new(pipe);
    let mut terminal_payload: Option<Value> = None;
    while let Ok(Some(line)) = splitter.next_line().await {
        watchdog.tick_stdout();
        let _ = writeln!(file, "{line}");
        if terminal_payload.is_none() {
            if let Some(value) = scan_run_terminal_line(&line) {
                let _ = capture::write_state_terminal_json(&visit_dir, &state_id, &value);
                terminal_payload = Some(value);
            }
        }
    }
    terminal_payload
}

async fn drain_stderr(pipe: tokio::process::ChildStderr, mut file: std::fs::File) {
    use std::io::Write as _;
    let mut splitter = LineSplitter::new(pipe);
    while let Ok(Some(line)) = splitter.next_line().await {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::canonical::test_helpers as h;
    use crate::workflow::canonical::{EdgeTarget, State, StateBody, StateMachine, Terminal};
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tempfile::TempDir;

    fn temp_runner(tmp: &Path, ghq: &str, ticket: &str) -> RealStateRunner {
        RealStateRunner {
            default_cli: "echo".into(),
            default_stall_seconds: 30,
            ticket_id: ticket.to_string(),
            ghq: ghq.to_string(),
            session_root: tmp.to_path_buf(),
            session_tempdir: tmp.join(ticket),
            cycle_id: Uuid::nil(),
            shutdown: crate::daemon::shutdown::ShutdownToken::new(),
            inflight: std::sync::Arc::new(crate::daemon::inflight::InflightRegistry::new()),
        }
    }

    fn empty_ctx() -> CycleContext {
        let mut ctx = CycleContext {
            globals: serde_json::Map::new(),
            visits: BTreeMap::new(),
            task_captures: BTreeMap::new(),
            iter: 0,
            max_iterations: 10,
        };
        ctx.globals
            .insert("ticket".into(), serde_json::json!({ "id": "ENG-1" }));
        ctx.globals.insert(
            "cycle".into(),
            serde_json::json!({ "id": "00000000-0000-0000-0000-000000000000", "kind": "rule", "trigger": "runtime", "iter": 0 }),
        );
        ctx.globals.insert(
            "config".into(),
            serde_json::json!({ "session_root": "/tmp/sess-x" }),
        );
        ctx
    }

    fn run_state_with_cmd(cmd: &str) -> State {
        let mut s = h::state("s", cmd);
        s.body = StateBody::Run {
            cmd: cmd.to_string(),
        };
        s.on_done = EdgeTarget::Terminal("__success__".into());
        s.on_fail = EdgeTarget::Terminal("__failure__".into());
        s
    }

    #[tokio::test]
    async fn exit_zero_no_sentinel_takes_on_done() {
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let runner = temp_runner(tmp.path(), "github.com/acme/x", "ENG-1");
        let state = run_state_with_cmd("true");
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");

        let outcome = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { runner.run_state(&state, &ctx).await },
        )
        .await;

        match outcome {
            StateOutcome::Edge { next, captures } => {
                assert_eq!(next, EdgeTarget::Terminal("__success__".into()));
                assert_eq!(captures.exit_code, 0);
                assert!(captures.directive.is_none());
            }
            other => panic!("expected Edge on_done, got {other:?}"),
        }
        // Capture artifacts written.
        let visit_dir = tmp
            .path()
            .join("ENG-1")
            .join(format!("cycle-{}", Uuid::nil()))
            .join("visit-1");
        assert!(visit_dir.join("s.stdout").is_file());
        assert!(visit_dir.join("s.exit_code").is_file());
    }

    #[tokio::test]
    async fn nonzero_exit_no_sentinel_takes_on_fail() {
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let runner = temp_runner(tmp.path(), "github.com/acme/x", "ENG-1");
        let state = run_state_with_cmd("false");
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");

        let outcome = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { runner.run_state(&state, &ctx).await },
        )
        .await;

        match outcome {
            StateOutcome::Edge { next, captures } => {
                assert_eq!(next, EdgeTarget::Terminal("__failure__".into()));
                assert_ne!(captures.exit_code, 0);
            }
            other => panic!("expected Edge on_fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sentinel_directive_resolves_to_default_terminal() {
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let runner = temp_runner(tmp.path(), "github.com/acme/x", "ENG-1");
        // Subprocess writes {"directive":"end"} to $ROKI_DIRECTIVE_PATH, then exits 0.
        let state = run_state_with_cmd(r#"echo '{"directive":"end"}' > "$ROKI_DIRECTIVE_PATH""#);
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");

        let outcome = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { runner.run_state(&state, &ctx).await },
        )
        .await;

        match outcome {
            StateOutcome::Edge { next, captures } => {
                assert_eq!(next, EdgeTarget::Terminal("__success__".into()));
                let directive = captures.directive.expect("directive present");
                assert_eq!(directive.directive, "end");
            }
            other => panic!("expected Edge to __success__, got {other:?}"),
        }
        let visit_dir = tmp
            .path()
            .join("ENG-1")
            .join(format!("cycle-{}", Uuid::nil()))
            .join("visit-1");
        assert!(visit_dir.join("s.directive.json").is_file());
    }

    #[tokio::test]
    async fn unparseable_sentinel_returns_unparseable_failure() {
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let runner = temp_runner(tmp.path(), "github.com/acme/x", "ENG-1");
        let state = run_state_with_cmd(r#"echo 'not-json' > "$ROKI_DIRECTIVE_PATH""#);
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");

        let outcome = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { runner.run_state(&state, &ctx).await },
        )
        .await;

        match outcome {
            StateOutcome::Failure { kind, .. } => assert_eq!(kind, FailureKind::Unparseable),
            other => panic!("expected Unparseable failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sentinel_directive_outside_legal_set_is_schema_drift() {
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let runner = temp_runner(tmp.path(), "github.com/acme/x", "ENG-1");
        let state =
            run_state_with_cmd(r#"echo '{"directive":"unknown_name"}' > "$ROKI_DIRECTIVE_PATH""#);
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");

        let outcome = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { runner.run_state(&state, &ctx).await },
        )
        .await;

        match outcome {
            StateOutcome::Failure { kind, error_text } => {
                assert_eq!(kind, FailureKind::SchemaDrift);
                assert!(error_text.contains("unknown_name"));
            }
            other => panic!("expected SchemaDrift failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn template_render_failure_in_cmd() {
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let runner = temp_runner(tmp.path(), "github.com/acme/x", "ENG-1");
        // Unmatched `{%` makes the Liquid parser fail before spawn.
        let state = run_state_with_cmd("echo {% if foo %}");
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");

        let outcome = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { runner.run_state(&state, &ctx).await },
        )
        .await;

        match outcome {
            StateOutcome::Failure { kind, .. } => assert_eq!(kind, FailureKind::TemplateError),
            other => panic!("expected TemplateError failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stall_detected_on_silent_subprocess() {
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let mut runner = temp_runner(tmp.path(), "github.com/acme/x", "ENG-1");
        runner.default_stall_seconds = 1;
        let mut state = run_state_with_cmd("sleep 30");
        state.timeout = Some(Duration::from_secs(1));
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");

        let outcome = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { runner.run_state(&state, &ctx).await },
        )
        .await;

        assert!(matches!(
            outcome,
            StateOutcome::Failure {
                kind: FailureKind::Stall,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cycle_state_with_real_runner_walks_two_states() {
        // Smoke test: glue cycle_state::run_cycle to RealStateRunner.
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();
        let runner = temp_runner(tmp.path(), "github.com/acme/x", "ENG-1");

        let mut a = run_state_with_cmd("true");
        a.id = "a".into();
        a.on_done = EdgeTarget::State("b".into());
        let mut b = run_state_with_cmd("true");
        b.id = "b".into();
        b.on_done = EdgeTarget::Terminal("__success__".into());

        let mut sm = StateMachine {
            start: "a".into(),
            states: BTreeMap::new(),
            terminals: BTreeMap::new(),
        };
        sm.states.insert("a".into(), a);
        sm.states.insert("b".into(), b);
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "success".into(),
            },
        );

        let mut ctx = empty_ctx();
        let result = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { crate::engine::cycle_state::run_cycle(&sm, &runner, &mut ctx).await },
        )
        .await
        .unwrap();
        assert_eq!(result.terminal_id, "__success__");
        assert_eq!(result.outcome, "success");
        assert_eq!(result.iterations, 2);
    }

    #[tokio::test]
    async fn shutdown_terminates_running_state_subprocess() {
        let tmp = TempDir::new().unwrap();
        let ghq_base = tmp.path().join("ghq");
        std::fs::create_dir_all(&ghq_base).unwrap();

        let shutdown = crate::daemon::shutdown::ShutdownToken::new();
        let inflight = std::sync::Arc::new(crate::daemon::inflight::InflightRegistry::new());

        let runner = RealStateRunner {
            default_cli: "echo".into(),
            default_stall_seconds: 60,
            ticket_id: "ENG-1".into(),
            ghq: "github.com/acme/x".into(),
            session_root: tmp.path().to_path_buf(),
            session_tempdir: tmp.path().join("ENG-1"),
            cycle_id: Uuid::nil(),
            shutdown: shutdown.clone(),
            inflight: inflight.clone(),
        };

        let state = run_state_with_cmd("sleep 30");
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");

        let shutdown_fire = shutdown.clone();
        let fire = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            shutdown_fire.fire();
        });

        let start = std::time::Instant::now();
        let outcome = temp_env::async_with_vars(
            [
                ("ROKI_GHQ_BASE_OVERRIDE", Some(ghq_base.to_str().unwrap())),
                ("ROKI_WT_ROOT_OVERRIDE", Some(tmp.path().to_str().unwrap())),
            ],
            async { runner.run_state(&state, &ctx).await },
        )
        .await;
        fire.await.unwrap();

        assert!(
            start.elapsed() < Duration::from_secs(15),
            "shutdown did not terminate the child quickly: {:?}",
            start.elapsed()
        );
        match outcome {
            StateOutcome::Failure { kind, .. } => {
                assert!(
                    matches!(kind, FailureKind::ProcessCrash),
                    "expected ProcessCrash, got {kind:?}"
                );
            }
            other => panic!("expected Failure(ProcessCrash), got {other:?}"),
        }
        assert!(inflight.snapshot().await.is_empty(), "registry not cleared");
    }

    #[test]
    fn split_frontmatter_returns_body_when_no_delimiter() {
        let raw = "no frontmatter here";
        let (fm, body) = split_frontmatter(raw);
        assert!(fm.is_none());
        assert_eq!(body, raw);
    }

    #[test]
    fn split_frontmatter_extracts_cli_field() {
        let raw = "---\ncli: claude --dangerously-skip-permissions\n---\nbody text\n";
        let (fm, body) = split_frontmatter(raw);
        let fm = fm.expect("frontmatter present");
        assert_eq!(
            extract_cli_field(fm).as_deref(),
            Some("claude --dangerously-skip-permissions")
        );
        assert_eq!(body, "body text\n");
    }

    #[test]
    fn state_id_to_env_uppercases_and_drops_invalid() {
        assert_eq!(state_id_to_env("judge"), "JUDGE");
        assert_eq!(state_id_to_env("impl-1"), "IMPL1");
        assert_eq!(state_id_to_env("a/b"), "AB");
        assert_eq!(state_id_to_env(""), "_");
    }

    #[test]
    fn build_env_emits_directive_path_and_state_namespace() {
        let state = run_state_with_cmd("true");
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");
        let path = std::path::PathBuf::from("/tmp/dir/s.1.json");
        let env = build_env(&state, &ctx, 1, &path);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"ROKI_DIRECTIVE_PATH"));
        assert!(names.contains(&"ROKI_STATE_ID"));
        assert!(names.contains(&"ROKI_STATE_VISITS"));
        assert!(names.contains(&"ROKI_TICKET_ID"));
        assert!(names.contains(&"ROKI_CYCLE_KIND"));
        assert!(names.contains(&"ROKI_CYCLE_ITER"));
        assert!(names.contains(&"ROKI_CONFIG_SESSION_ROOT"));
        // ROKI_API_URL is only set when the test fixture configures [api].port.
        // The default fixture leaves the API server disabled, so assert absence
        // to lock the gating behavior.
        assert!(!names.contains(&"ROKI_API_URL"));
    }

    #[test]
    fn build_env_emits_roki_api_url_when_api_namespace_present() {
        let state = run_state_with_cmd("true");
        let mut ctx = empty_ctx();
        ctx.globals.insert(
            "api".into(),
            serde_json::json!({ "url": "http://127.0.0.1:7777" }),
        );
        ctx.bump_visit("s");
        let path = std::path::PathBuf::from("/tmp/dir/s.1.json");
        let env = build_env(&state, &ctx, 1, &path);
        let pair = env
            .iter()
            .find(|(k, _)| k == "ROKI_API_URL")
            .expect("ROKI_API_URL present");
        assert_eq!(pair.1, "http://127.0.0.1:7777");
    }

    #[test]
    fn build_env_flattens_task_captures() {
        let state = run_state_with_cmd("true");
        let mut ctx = empty_ctx();
        ctx.bump_visit("s");
        ctx.record_capture(
            "judge",
            TaskCaptures {
                exit_code: 0,
                duration_seconds: 12,
                directive: Some(DirectivePayload {
                    directive: "end".into(),
                    outcome: None,
                    extra: {
                        let mut m = serde_json::Map::new();
                        m.insert("verdict".into(), serde_json::json!("ok"));
                        m
                    },
                }),
                terminal: None,
            },
        );
        let path = std::path::PathBuf::from("/tmp/dir/s.1.json");
        let env = build_env(&state, &ctx, 1, &path);
        let pairs: std::collections::HashMap<String, String> = env.into_iter().collect();
        assert_eq!(
            pairs.get("ROKI_TASK_JUDGE_EXIT_CODE").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            pairs
                .get("ROKI_TASK_JUDGE_DURATION_SECONDS")
                .map(String::as_str),
            Some("12")
        );
        assert_eq!(
            pairs
                .get("ROKI_TASK_JUDGE_DIRECTIVE_VERDICT")
                .map(String::as_str),
            Some("ok"),
        );
    }
}
