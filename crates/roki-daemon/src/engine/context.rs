//! PhaseContext: Liquid object + ROKI_* env builder.
//!
//! Every phase invocation rebuilds the Liquid object from the current
//! context state via `serde_json::to_value`. The env builder produces
//! `(name, value)` pairs scoped to the current phase: ticket / repo / cycle
//! / config fields are always exported; pre / post / run fields are only
//! exported when populated. Top-level scalars from pre/post payloads are
//! exported as `ROKI_PRE_<KEY>` / `ROKI_POST_<KEY>` per FR 01 §Inter-phase
//! data flow.

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::linear::ticket::NormalizedTicket;

#[derive(Debug, Clone, Serialize)]
pub struct TicketView {
    pub id: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignee: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoView {
    pub ghq: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CycleView {
    pub id: String,
    pub kind: &'static str,
    pub trigger: &'static str,
    pub iter: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigView {
    pub max_iterations: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub exit_code: i32,
    pub duration_seconds: u64,
    /// `Some(value)` iff `iter-N/run.terminal.json` was written for the
    /// current iter (claude/codex `result` event surfaced).
    pub terminal: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureView {
    pub kind: String,
    pub phase: String,
    pub iter: u32,
    /// Stringified for Liquid; absent in env when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub error_text: String,
    pub failed_cycle_id: String,
}

/// Engine-side execution context. Mutated through `set_iter`, `set_pre`,
/// `set_post`, `set_run` between phase invocations.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseContext {
    pub ticket: TicketView,
    pub repo: RepoView,
    pub cycle: CycleView,
    pub config: ConfigView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureView>,
}

impl PhaseContext {
    pub fn new(
        admitted: &AdmittedTicket,
        cycle_id: Uuid,
        cfg: &RokiConfig,
        cycle_kind: crate::engine::outcome::CycleKind,
    ) -> Self {
        Self {
            ticket: TicketView::from(&admitted.ticket),
            repo: RepoView {
                ghq: admitted.ghq.clone(),
            },
            cycle: CycleView {
                id: cycle_id.to_string(),
                kind: cycle_kind.as_str(),
                trigger: "runtime",
                iter: 0,
            },
            config: ConfigView {
                max_iterations: cfg.engine.max_iterations,
            },
            pre: None,
            post: None,
            run: None,
            failure: None,
        }
    }

    pub fn set_failure(&mut self, meta: crate::engine::outcome::FailureMeta) {
        self.failure = Some(FailureView {
            kind: meta.kind.as_str().to_string(),
            phase: meta.phase.as_str().to_string(),
            iter: meta.iter,
            exit_code: meta.exit_code,
            error_text: meta.error_text,
            failed_cycle_id: meta.failed_cycle_id.to_string(),
        });
    }

    pub fn set_iter(&mut self, iter: u32) {
        self.cycle.iter = iter;
        self.run = None;
    }

    pub fn set_pre(&mut self, payload: Value) {
        self.pre = Some(payload);
    }

    pub fn set_post(&mut self, payload: Value) {
        self.post = Some(payload);
    }

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
}

impl From<&NormalizedTicket> for TicketView {
    fn from(ticket: &NormalizedTicket) -> Self {
        Self {
            id: ticket.id.clone(),
            title: ticket.title.clone(),
            body: ticket.body.clone(),
            labels: ticket.labels.clone(),
            assignee: ticket.assignee_id.clone(),
            status: ticket.status.clone(),
        }
    }
}

/// Build the `ROKI_*` env pairs the phase subprocess receives.
///
/// Returns `(name, value)` tuples ready for `Command::envs`.
pub fn roki_env_pairs(ctx: &PhaseContext) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = vec![
        ("ROKI_TICKET_ID".to_string(), ctx.ticket.id.clone()),
        ("ROKI_REPO".to_string(), ctx.repo.ghq.clone()),
        ("ROKI_CYCLE_ID".to_string(), ctx.cycle.id.clone()),
        ("ROKI_CYCLE_KIND".to_string(), ctx.cycle.kind.to_string()),
        (
            "ROKI_CYCLE_TRIGGER".to_string(),
            ctx.cycle.trigger.to_string(),
        ),
        ("ROKI_CYCLE_ITER".to_string(), ctx.cycle.iter.to_string()),
        (
            "ROKI_CONFIG_MAX_ITERATIONS".to_string(),
            ctx.config.max_iterations.to_string(),
        ),
    ];

    if let Some(payload) = ctx.pre.as_ref() {
        push_payload_scalars(&mut pairs, "ROKI_PRE_", payload);
    }
    if let Some(payload) = ctx.post.as_ref() {
        push_payload_scalars(&mut pairs, "ROKI_POST_", payload);
    }
    if let Some(run) = ctx.run.as_ref() {
        pairs.push(("ROKI_RUN_EXIT_CODE".to_string(), run.exit_code.to_string()));
        pairs.push((
            "ROKI_RUN_DURATION_SECONDS".to_string(),
            run.duration_seconds.to_string(),
        ));
    }

    if let Some(f) = &ctx.failure {
        pairs.push(("ROKI_FAILURE_KIND".to_string(), f.kind.clone()));
        pairs.push(("ROKI_FAILURE_PHASE".to_string(), f.phase.clone()));
        pairs.push(("ROKI_FAILURE_ITER".to_string(), f.iter.to_string()));
        if let Some(ec) = f.exit_code {
            pairs.push(("ROKI_FAILURE_EXIT_CODE".to_string(), ec.to_string()));
        }
        pairs.push(("ROKI_FAILURE_ERROR_TEXT".to_string(), f.error_text.clone()));
        pairs.push((
            "ROKI_FAILURE_FAILED_CYCLE_ID".to_string(),
            f.failed_cycle_id.clone(),
        ));
    }

    pairs
}

fn push_payload_scalars(pairs: &mut Vec<(String, String)>, prefix: &str, payload: &Value) {
    let Some(map) = payload.as_object() else {
        return;
    };
    for (key, value) in map {
        let scalar = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            // Nested objects, arrays, and null are reachable through Liquid
            // (`{{ pre.foo.bar }}`) but never through env.
            _ => continue,
        };
        let upper = key.to_ascii_uppercase();
        if !upper.bytes().all(is_legal_env_char) {
            tracing::info!(
                key = %key,
                "ROKI_{prefix}* skip: key '{key}' has non [A-Z0-9_] characters",
                prefix = prefix
            );
            continue;
        }
        pairs.push((format!("{prefix}{upper}"), scalar));
    }
}

fn is_legal_env_char(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

/// Convert the context into a Liquid object (`liquid::Object`) for use as the
/// render globals.
pub fn to_liquid_object(ctx: &PhaseContext) -> liquid::Object {
    // serde_json -> liquid value via the liquid integration. Round-tripping
    // through serde_json is the simplest path that respects the existing
    // serde derives on the view types.
    let value = serde_json::to_value(ctx).expect("PhaseContext serialises");
    match value {
        Value::Object(map) => map
            .into_iter()
            .map(|(k, v)| {
                (
                    k.into(),
                    liquid::model::to_value(&v).unwrap_or(liquid::model::Value::Nil),
                )
            })
            .collect(),
        _ => liquid::Object::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> NormalizedTicket {
        NormalizedTicket::new(
            "ENG-1".to_string(),
            Some("u1".to_string()),
            "in_progress".to_string(),
            vec!["bug".to_string()],
            "Title".to_string(),
            "Body".to_string(),
        )
    }

    fn admitted() -> AdmittedTicket {
        AdmittedTicket {
            ticket: ticket(),
            ghq: "github.com/acme/widget".to_string(),
        }
    }

    fn cfg(max_iterations: u32) -> RokiConfig {
        // Build a minimal RokiConfig in-test. Reach into the public fields
        // directly; Default + struct literal is sufficient.
        use crate::config::roki::*;
        use std::path::PathBuf;
        RokiConfig {
            linear: LinearSection {
                token: "x".to_string(),
            },
            linear_webhook: LinearWebhookSection {
                bind: "127.0.0.1".to_string(),
                port: 8000,
                secret: None,
            },
            default_ai_command: DefaultAiCommandSection {
                cli: "echo".to_string(),
                stall_seconds: 300,
            },
            engine: EngineSection { max_iterations },
            paths: PathsSection {
                workflow: PathBuf::from("/tmp/w"),
                session_root: PathBuf::from("/tmp/s"),
            },
            log: LogSection::default(),
            default_ai_session: None,
        }
    }

    #[test]
    fn env_pairs_include_ticket_repo_cycle_config_at_iter_zero() {
        let ctx = PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(7),
            crate::engine::outcome::CycleKind::Rule,
        );
        let pairs = roki_env_pairs(&ctx);
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "ROKI_TICKET_ID" && v == "ENG-1")
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "ROKI_REPO" && v == "github.com/acme/widget")
        );
        assert!(pairs.iter().any(|(k, _v)| k == "ROKI_CYCLE_ID"));
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "ROKI_CYCLE_KIND" && v == "rule")
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "ROKI_CYCLE_TRIGGER" && v == "runtime")
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "ROKI_CYCLE_ITER" && v == "0")
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "ROKI_CONFIG_MAX_ITERATIONS" && v == "7")
        );
    }

    #[test]
    fn env_pairs_export_pre_top_level_scalars_only() {
        let mut ctx = PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(10),
            crate::engine::outcome::CycleKind::Rule,
        );
        ctx.set_pre(serde_json::json!({
            "directive": "run",
            "outcome": "success",
            "count": 3,
            "ready": true,
            "nested": {"inner": "x"},
            "list": [1, 2]
        }));
        let pairs = roki_env_pairs(&ctx);
        let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"ROKI_PRE_DIRECTIVE"));
        assert!(names.contains(&"ROKI_PRE_OUTCOME"));
        assert!(names.contains(&"ROKI_PRE_COUNT"));
        assert!(names.contains(&"ROKI_PRE_READY"));
        // Nested objects and arrays must be skipped.
        assert!(!names.contains(&"ROKI_PRE_NESTED"));
        assert!(!names.contains(&"ROKI_PRE_LIST"));
    }

    #[test]
    fn env_pairs_skip_keys_with_non_ascii_chars() {
        let mut ctx = PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(10),
            crate::engine::outcome::CycleKind::Rule,
        );
        ctx.set_pre(serde_json::json!({
            "directive": "run",
            "my-field": "x", // hyphen — uppercase is "MY-FIELD", '-' is not legal.
        }));
        let pairs = roki_env_pairs(&ctx);
        let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"ROKI_PRE_DIRECTIVE"));
        assert!(!names.iter().any(|n| n.contains("MY-FIELD")));
    }

    #[test]
    fn env_pairs_export_run_exit_code_and_duration() {
        let mut ctx = PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(10),
            crate::engine::outcome::CycleKind::Rule,
        );
        ctx.set_run(7, 42, None);
        let pairs = roki_env_pairs(&ctx);
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "ROKI_RUN_EXIT_CODE" && v == "7")
        );
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "ROKI_RUN_DURATION_SECONDS" && v == "42")
        );
    }

    #[test]
    fn run_terminal_exposed_via_liquid() {
        let mut ctx = PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(10),
            crate::engine::outcome::CycleKind::Rule,
        );
        let terminal = serde_json::json!({"is_error": false, "result": "ok"});
        ctx.set_run(0, 12, Some(terminal));
        let rendered = crate::engine::template::render_str(
            "{{ run.terminal.is_error }}/{{ run.terminal.result }}",
            &ctx,
        )
        .unwrap();
        assert_eq!(rendered, "false/ok");
    }

    #[test]
    fn run_terminal_clears_between_iters() {
        let mut ctx = PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(10),
            crate::engine::outcome::CycleKind::Rule,
        );
        ctx.set_run(0, 1, Some(serde_json::json!({"is_error": false})));
        ctx.set_iter(2);
        let rendered =
            crate::engine::template::render_str("{{ run.terminal.is_error }}", &ctx).unwrap();
        assert_eq!(rendered, "");
    }

    #[test]
    fn liquid_object_carries_ticket_repo_and_cycle_iter() {
        let mut ctx = PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(10),
            crate::engine::outcome::CycleKind::Rule,
        );
        ctx.set_iter(3);
        let obj = to_liquid_object(&ctx);
        // Values are nested liquid Objects; project to JSON for cheap assertions.
        let json = serde_json::to_value(&obj).unwrap();
        assert_eq!(json["ticket"]["id"], "ENG-1");
        assert_eq!(json["repo"]["ghq"], "github.com/acme/widget");
        assert_eq!(json["cycle"]["iter"], 3);
        assert_eq!(json["config"]["max_iterations"], 10);
    }

    #[test]
    fn phase_context_cycle_kind_failure() {
        use crate::engine::outcome::CycleKind;
        let ctx = PhaseContext::new(&admitted(), uuid::Uuid::nil(), &cfg(5), CycleKind::Failure);
        assert_eq!(ctx.cycle.kind, "failure");

        let env: Vec<(String, String)> = roki_env_pairs(&ctx).into_iter().collect();
        assert!(
            env.iter()
                .any(|(k, v)| k == "ROKI_CYCLE_KIND" && v == "failure")
        );
    }

    #[test]
    fn phase_context_failure_view_populated() {
        use crate::engine::outcome::{CycleKind, FailureKind, FailureMeta, PhaseKind};
        let failed_cycle_id = uuid::Uuid::from_u128(42);
        let meta = FailureMeta {
            failed_cycle_id,
            kind: FailureKind::Unparseable,
            phase: PhaseKind::Post,
            iter: 3,
            exit_code: Some(0),
            error_text: "no JSON object on stdout".into(),
        };
        let mut ctx =
            PhaseContext::new(&admitted(), uuid::Uuid::nil(), &cfg(5), CycleKind::Failure);
        ctx.set_failure(meta);

        let env: std::collections::HashMap<String, String> =
            roki_env_pairs(&ctx).into_iter().collect();
        assert_eq!(env.get("ROKI_FAILURE_KIND").unwrap(), "unparseable");
        assert_eq!(env.get("ROKI_FAILURE_PHASE").unwrap(), "post");
        assert_eq!(env.get("ROKI_FAILURE_ITER").unwrap(), "3");
        assert_eq!(env.get("ROKI_FAILURE_EXIT_CODE").unwrap(), "0");
        assert_eq!(
            env.get("ROKI_FAILURE_ERROR_TEXT").unwrap(),
            "no JSON object on stdout"
        );
        assert_eq!(
            env.get("ROKI_FAILURE_FAILED_CYCLE_ID").unwrap(),
            &failed_cycle_id.to_string()
        );
    }

    #[test]
    fn phase_context_failure_absent_for_rule_cycle() {
        use crate::engine::outcome::CycleKind;
        let ctx = PhaseContext::new(&admitted(), uuid::Uuid::nil(), &cfg(5), CycleKind::Rule);
        let env: std::collections::HashMap<String, String> =
            roki_env_pairs(&ctx).into_iter().collect();
        assert!(!env.contains_key("ROKI_FAILURE_KIND"));
    }

    #[test]
    fn phase_context_failure_exit_code_absent_when_none() {
        use crate::engine::outcome::{CycleKind, FailureKind, FailureMeta, PhaseKind};
        let meta = FailureMeta {
            failed_cycle_id: uuid::Uuid::nil(),
            kind: FailureKind::Stall,
            phase: PhaseKind::Run,
            iter: 1,
            exit_code: None,
            error_text: "stall".into(),
        };
        let mut ctx =
            PhaseContext::new(&admitted(), uuid::Uuid::nil(), &cfg(5), CycleKind::Failure);
        ctx.set_failure(meta);
        let env: std::collections::HashMap<String, String> =
            roki_env_pairs(&ctx).into_iter().collect();
        assert!(!env.contains_key("ROKI_FAILURE_EXIT_CODE"));
    }
}
