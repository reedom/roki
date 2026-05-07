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
}

impl PhaseContext {
    pub fn new(admitted: &AdmittedTicket, cycle_id: Uuid, cfg: &RokiConfig) -> Self {
        Self {
            ticket: TicketView::from(&admitted.ticket),
            repo: RepoView {
                ghq: admitted.ghq.clone(),
            },
            cycle: CycleView {
                id: cycle_id.to_string(),
                kind: "rule",
                trigger: "runtime",
                iter: 0,
            },
            config: ConfigView {
                max_iterations: cfg.engine.max_iterations,
            },
            pre: None,
            post: None,
            run: None,
        }
    }

    pub fn set_iter(&mut self, iter: u32) {
        self.cycle.iter = iter;
    }

    pub fn set_pre(&mut self, payload: Value) {
        self.pre = Some(payload);
    }

    pub fn set_post(&mut self, payload: Value) {
        self.post = Some(payload);
    }

    pub fn set_run(&mut self, exit_code: i32, duration_seconds: u64) {
        self.run = Some(RunView {
            exit_code,
            duration_seconds,
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
        pairs.push((
            "ROKI_RUN_EXIT_CODE".to_string(),
            run.exit_code.to_string(),
        ));
        pairs.push((
            "ROKI_RUN_DURATION_SECONDS".to_string(),
            run.duration_seconds.to_string(),
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
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(7));
        let pairs = roki_env_pairs(&ctx);
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_TICKET_ID" && v == "ENG-1"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_REPO" && v == "github.com/acme/widget"));
        assert!(pairs.iter().any(|(k, _v)| k == "ROKI_CYCLE_ID"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_CYCLE_KIND" && v == "rule"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_CYCLE_TRIGGER" && v == "runtime"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_CYCLE_ITER" && v == "0"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_CONFIG_MAX_ITERATIONS" && v == "7"));
    }

    #[test]
    fn env_pairs_export_pre_top_level_scalars_only() {
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(10));
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
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(10));
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
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(10));
        ctx.set_run(7, 42);
        let pairs = roki_env_pairs(&ctx);
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_RUN_EXIT_CODE" && v == "7"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_RUN_DURATION_SECONDS" && v == "42"));
    }

    #[test]
    fn liquid_object_carries_ticket_repo_and_cycle_iter() {
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(10));
        ctx.set_iter(3);
        let obj = to_liquid_object(&ctx);
        // Values are nested liquid Objects; project to JSON for cheap assertions.
        let json = serde_json::to_value(&obj).unwrap();
        assert_eq!(json["ticket"]["id"], "ENG-1");
        assert_eq!(json["repo"]["ghq"], "github.com/acme/widget");
        assert_eq!(json["cycle"]["iter"], 3);
        assert_eq!(json["config"]["max_iterations"], 10);
    }
}
