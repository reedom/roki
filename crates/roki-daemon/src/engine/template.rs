//! Liquid render of argv strings and stdin bodies.
//!
//! The same `render_str` API serves all render channels: argv (the
//! pre-shell-words cli line), stdin body (path body, inline prompt), and
//! the inline cmd string. Failures map to `FailureKind::TemplateError` at
//! the call site (`engine::phase`).

use liquid::model::{DisplayCow, KStringCow, ObjectView, State, Value, ValueView};
use thiserror::Error;

use super::context::{PhaseContext, to_liquid_object};

/// Render error wrapper. The engine maps this to `FailureKind::TemplateError`
/// when surfacing it through `PhaseOutcome`.
#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("template parse failed: {0}")]
    Parse(String),
    #[error("template render failed: {0}")]
    Render(String),
}

/// A thin `ObjectView` wrapper around a populated `liquid::Object` that also
/// exposes a fixed set of optional top-level keys as `NilSection` when those
/// keys are absent from the underlying map.
///
/// Liquid 0.26 raises "Unknown variable" for any top-level key that is
/// missing from globals. By always advertising these keys we prevent that
/// error; `NilSection` then absorbs any sub-key access and returns `Nil`
/// (empty string) rather than raising "Unknown index".
///
/// Sentinel keys span both the legacy phase model (`pre`, `post`, `run`) and
/// the slice-8 state-machine model (`state`, `failure`, `tasks`). The unified
/// list lets one render path serve both contexts during the migration.
struct LenientGlobals {
    inner: liquid::Object,
    /// Sentinel sections advertised as present when absent from `inner`.
    sentinel_keys: &'static [&'static str],
    nil_section: NilSection,
}

impl LenientGlobals {
    fn new(inner: liquid::Object) -> Self {
        Self {
            inner,
            sentinel_keys: &["pre", "post", "run", "state", "failure", "tasks"],
            nil_section: NilSection::new(),
        }
    }

    fn is_sentinel(&self, key: &str) -> bool {
        self.sentinel_keys.contains(&key)
    }
}

impl std::fmt::Debug for LenientGlobals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LenientGlobals").finish()
    }
}

impl ValueView for LenientGlobals {
    fn as_debug(&self) -> &dyn std::fmt::Debug {
        self
    }
    fn render(&self) -> DisplayCow<'_> {
        self.inner.render()
    }
    fn source(&self) -> DisplayCow<'_> {
        self.inner.source()
    }
    fn type_name(&self) -> &'static str {
        "object"
    }
    fn query_state(&self, state: State) -> bool {
        self.inner.query_state(state)
    }
    fn to_kstr(&self) -> KStringCow<'_> {
        self.inner.to_kstr()
    }
    fn to_value(&self) -> Value {
        self.inner.to_value()
    }
    fn as_object(&self) -> Option<&dyn ObjectView> {
        Some(self)
    }
}

impl ObjectView for LenientGlobals {
    fn as_value(&self) -> &dyn ValueView {
        self
    }
    fn size(&self) -> i64 {
        self.inner.size()
    }
    fn keys<'k>(&'k self) -> Box<dyn Iterator<Item = KStringCow<'k>> + 'k> {
        Box::new(self.inner.keys().map(|k| KStringCow::from(k.as_str())))
    }
    fn values<'k>(&'k self) -> Box<dyn Iterator<Item = &'k dyn ValueView> + 'k> {
        Box::new(self.inner.values().map(|v| v.as_view()))
    }
    fn iter<'k>(&'k self) -> Box<dyn Iterator<Item = (KStringCow<'k>, &'k dyn ValueView)> + 'k> {
        Box::new(
            self.inner
                .iter()
                .map(|(k, v)| (KStringCow::from(k.as_str()), v.as_view())),
        )
    }
    fn contains_key(&self, index: &str) -> bool {
        self.inner.contains_key(index) || self.is_sentinel(index)
    }
    fn get<'s>(&'s self, index: &str) -> Option<&'s dyn ValueView> {
        if let Some(v) = self.inner.get(index) {
            return Some(v.as_view());
        }
        if self.is_sentinel(index) {
            return Some(&self.nil_section);
        }
        None
    }
}

/// A Liquid object section that silently returns `Nil` for any sub-key access.
///
/// Liquid 0.26 raises "Unknown index" when a key is absent from an object during
/// `{{ obj.field }}` expansion. `NilSection` satisfies every `get()` call with
/// `&Value::Nil` so that absent optional sections (e.g. `pre` before the pre
/// phase has run) expand to empty string rather than aborting the render.
struct NilSection {
    nil: Value,
}

impl NilSection {
    fn new() -> Self {
        Self { nil: Value::Nil }
    }
}

impl std::fmt::Debug for NilSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NilSection").finish()
    }
}

impl ValueView for NilSection {
    fn as_debug(&self) -> &dyn std::fmt::Debug {
        self
    }
    fn render(&self) -> DisplayCow<'_> {
        self.nil.render()
    }
    fn source(&self) -> DisplayCow<'_> {
        self.nil.source()
    }
    fn type_name(&self) -> &'static str {
        "object"
    }
    fn query_state(&self, state: State) -> bool {
        match state {
            State::Truthy => false,
            State::DefaultValue | State::Empty | State::Blank => true,
        }
    }
    fn to_kstr(&self) -> KStringCow<'_> {
        KStringCow::from_static("")
    }
    fn to_value(&self) -> Value {
        Value::Object(liquid::Object::new())
    }
    fn as_object(&self) -> Option<&dyn ObjectView> {
        Some(self)
    }
}

impl ObjectView for NilSection {
    fn as_value(&self) -> &dyn ValueView {
        self
    }
    fn size(&self) -> i64 {
        0
    }
    fn keys<'k>(&'k self) -> Box<dyn Iterator<Item = KStringCow<'k>> + 'k> {
        Box::new(std::iter::empty())
    }
    fn values<'k>(&'k self) -> Box<dyn Iterator<Item = &'k dyn ValueView> + 'k> {
        Box::new(std::iter::empty())
    }
    fn iter<'k>(&'k self) -> Box<dyn Iterator<Item = (KStringCow<'k>, &'k dyn ValueView)> + 'k> {
        Box::new(std::iter::empty())
    }
    fn contains_key(&self, _index: &str) -> bool {
        // Always report the key as present so `find` calls `get` and receives
        // `Nil` rather than taking the "Unknown index" error path.
        true
    }
    fn get<'s>(&'s self, _index: &str) -> Option<&'s dyn ValueView> {
        // Return self (rather than Value::Nil) so that nested key access like
        // `run.terminal.is_error` keeps walking through NilSection at every
        // level — Liquid would otherwise raise "Unknown index" when indexing
        // into Nil. The terminal render still produces an empty string because
        // NilSection's `to_kstr` returns "".
        Some(self)
    }
}

/// Render `template` against `ctx`'s Liquid object. Missing variables expand
/// to the Liquid default (empty string) per Shopify Liquid semantics.
pub fn render_str(template: &str, ctx: &PhaseContext) -> Result<String, TemplateError> {
    render_str_with_globals(template, &to_liquid_object(ctx))
}

/// Render `template` against an already-built Liquid object. Used by the
/// slice-8 state runner to pass a `CycleContext`-derived globals map without
/// going through `PhaseContext`.
pub fn render_str_with_globals(
    template: &str,
    globals: &liquid::Object,
) -> Result<String, TemplateError> {
    let parser = liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(|err| TemplateError::Parse(err.to_string()))?;
    let parsed = parser
        .parse(template)
        .map_err(|err| TemplateError::Parse(err.to_string()))?;
    let lenient = LenientGlobals::new(globals.clone());
    parsed
        .render(&lenient)
        .map_err(|err| TemplateError::Render(err.to_string()))
}

/// Evaluate a Liquid expression as a boolean. The expression is wrapped in
/// `{% if <expr> %}1{% endif %}`; the render result is `"1"` for truthy and
/// `""` for falsy. Used by the cycle driver for `state.if_cond` skip.
pub fn eval_cond(expr: &str, globals: &liquid::Object) -> Result<bool, TemplateError> {
    let wrapped = format!("{{% if {expr} %}}1{{% endif %}}");
    let rendered = render_str_with_globals(&wrapped, globals)?;
    Ok(rendered == "1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::AdmittedTicket;
    use crate::config::roki::*;
    use crate::engine::context::CycleTrigger;
    use crate::engine::outcome::CycleKind;
    use crate::linear::ticket::NormalizedTicket;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn admitted() -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "ENG-7".to_string(),
                Some("u1".to_string()),
                "review".to_string(),
                vec!["needs-impl".to_string()],
                "Implement widget".to_string(),
                "Body".to_string(),
            ),
            ghq: "github.com/acme/widget".to_string(),
        }
    }

    fn cfg() -> RokiConfig {
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
            engine: EngineSection {
                max_iterations: 10,
                shutdown_window_seconds: 30,
            },
            paths: PathsSection {
                workflow: PathBuf::from("/tmp/w"),
                session_root: PathBuf::from("/tmp/s"),
            },
            log: LogSection::default(),
            escalation: EscalationSection::default(),
            default_ai_session: None,
        }
    }

    #[test]
    fn renders_ticket_id_and_iter() {
        let mut ctx = super::PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(),
            CycleKind::Rule,
            CycleTrigger::Runtime,
        );
        ctx.set_iter(2);
        let out = render_str("ticket {{ ticket.id }} iter {{ cycle.iter }}", &ctx).unwrap();
        assert_eq!(out, "ticket ENG-7 iter 2");
    }

    #[test]
    fn renders_pre_payload_field() {
        let mut ctx = super::PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(),
            CycleKind::Rule,
            CycleTrigger::Runtime,
        );
        ctx.set_pre(serde_json::json!({"directive":"run","note":"hello"}));
        let out = render_str("pre note: {{ pre.note }}", &ctx).unwrap();
        assert_eq!(out, "pre note: hello");
    }

    #[test]
    fn missing_variable_expands_to_empty_string() {
        let ctx = super::PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(),
            CycleKind::Rule,
            CycleTrigger::Runtime,
        );
        // `pre` is None at iter 0 before any pre runs; the dereference returns nil.
        let out = render_str("got [{{ pre.note }}]", &ctx).unwrap();
        assert_eq!(out, "got []");
    }

    #[test]
    fn parse_error_returns_template_error() {
        let ctx = super::PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(),
            CycleKind::Rule,
            CycleTrigger::Runtime,
        );
        // Unmatched `{%` confuses the parser.
        let result = render_str("{% if foo %}", &ctx);
        match result {
            Err(TemplateError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn render_with_globals_renders_state_namespace() {
        let mut obj = liquid::Object::new();
        let mut state = liquid::Object::new();
        state.insert("id".into(), liquid::model::Value::scalar("judge"));
        state.insert("visit_n".into(), liquid::model::Value::scalar(2_i64));
        obj.insert("state".into(), liquid::model::Value::Object(state));
        let out = render_str_with_globals("state {{ state.id }} visit {{ state.visit_n }}", &obj)
            .unwrap();
        assert_eq!(out, "state judge visit 2");
    }

    #[test]
    fn render_with_globals_treats_absent_state_as_nil() {
        let obj = liquid::Object::new();
        let out = render_str_with_globals("[{{ state.id }}]", &obj).unwrap();
        assert_eq!(out, "[]");
    }

    #[test]
    fn render_with_globals_treats_absent_failure_and_tasks_as_nil() {
        let obj = liquid::Object::new();
        let out = render_str_with_globals("[{{ failure.kind }}|{{ tasks.judge.exit_code }}]", &obj)
            .unwrap();
        assert_eq!(out, "[|]");
    }

    #[test]
    fn eval_cond_truthy_string_returns_true() {
        let mut obj = liquid::Object::new();
        obj.insert("flag".into(), liquid::model::Value::scalar("yes"));
        assert!(eval_cond("flag", &obj).unwrap());
    }

    #[test]
    fn eval_cond_falsy_when_var_absent() {
        let obj = liquid::Object::new();
        // Liquid treats absent variables as nil → falsy in `{% if %}`.
        assert!(!eval_cond("ghost", &obj).unwrap());
    }

    #[test]
    fn eval_cond_compares_values() {
        let mut obj = liquid::Object::new();
        let mut tasks = liquid::Object::new();
        let mut judge = liquid::Object::new();
        judge.insert("exit_code".into(), liquid::model::Value::scalar(0_i64));
        tasks.insert("judge".into(), liquid::model::Value::Object(judge));
        obj.insert("tasks".into(), liquid::model::Value::Object(tasks));
        assert!(eval_cond("tasks.judge.exit_code == 0", &obj).unwrap());
        assert!(!eval_cond("tasks.judge.exit_code == 1", &obj).unwrap());
    }

    #[test]
    fn eval_cond_parse_error_returns_template_error() {
        let obj = liquid::Object::new();
        // Unmatched braces inside the expression confuse the parser.
        let result = eval_cond("flag and {%", &obj);
        assert!(result.is_err());
    }

    #[test]
    fn renders_run_exit_code_when_set() {
        let mut ctx = super::PhaseContext::new(
            &admitted(),
            Uuid::nil(),
            &cfg(),
            CycleKind::Rule,
            CycleTrigger::Runtime,
        );
        ctx.set_run(5, 12, None);
        let out = render_str(
            "exit={{ run.exit_code }} dur={{ run.duration_seconds }}",
            &ctx,
        )
        .unwrap();
        assert_eq!(out, "exit=5 dur=12");
    }
}
