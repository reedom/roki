// Walking-skeleton tasks land in dependency order: this loader (task 2.2)
// precedes the runtime wiring that calls `WorkflowConfig::load`. Until that
// wiring lands, the loader and its helpers are exercised only by the unit
// tests below, which triggers `dead_code` for the leaf API. Allow it
// module-locally instead of leaking the relaxation crate-wide.
#![allow(dead_code)]

//! `WORKFLOW.toml` loader for the walking-skeleton daemon.
//!
//! Reads the minimal slice required by the skeleton: `[admission].assignee`,
//! the first `[[admission.repos]]` entry's `ghq` (or `None` when the array is
//! empty/missing), and the `[[rule]]` array with `when.status` +
//! `when.labels.has_all` + optional `pre`, required `run`, optional `post`
//! phase bodies per design `config::workflow`.
//!
//! Validation is strict: any other `when.*` key, any unknown phase-body key,
//! the `session` phase shape, or missing `run` fails the load with a
//! key-path-bearing error before the binary binds the listener (Req 5.3,
//! 6.2).
//!
//! Presence of `[[cleanup]]`, `[[on_failure]]`, and per-repo
//! `[[admission.repos]] workflow` overrides is tolerated without evaluation
//! per Req 2.5.

use std::path::Path;

use toml::Value;

use crate::error::WorkflowError;

/// Top-level loaded workflow configuration.
#[derive(Clone, Debug)]
pub struct WorkflowConfig {
    pub admission: AdmissionSection,
    /// First `[[admission.repos]]` entry; `None` when the array is empty or
    /// missing (admission surfaces `NoRepos` per Req 4.4).
    pub repo: Option<AdmissionRepo>,
    pub rules: Vec<Rule>,
}

/// `[admission]` section.
#[derive(Clone, Debug)]
pub struct AdmissionSection {
    pub assignee: String,
}

/// First `[[admission.repos]]` entry.
///
/// Only `ghq` is consulted at the skeleton level; per-entry `when.*` and
/// `workflow` overrides are tolerated without evaluation per Req 2.5.
#[derive(Clone, Debug)]
pub struct AdmissionRepo {
    pub ghq: String,
}

/// One `[[rule]]` entry. Restricts to command-shape phases per slice 1; the
/// `session` shape is rejected at load time.
#[derive(Clone, Debug)]
pub struct Rule {
    pub when_status: String,
    pub when_labels_has_all: Vec<String>,
    pub pre: Option<crate::engine::outcome::PhaseBody>,
    pub run: crate::engine::outcome::PhaseBody,
    pub post: Option<crate::engine::outcome::PhaseBody>,
}

impl WorkflowConfig {
    /// Load and validate `WORKFLOW.toml` from `path`.
    ///
    /// Returns `WorkflowError::MissingFile` when the file is absent,
    /// `Unreadable` for I/O errors, `Parse` for TOML syntax errors,
    /// `MissingField` when a required field is absent, `UnsupportedWhen`
    /// for any `when.*` key beyond `when.status` + `when.labels.has_all`,
    /// and `UnsupportedRunForm` for `run.path` / `run.prompt`, missing
    /// `run`, missing `run.cmd`, or `pre.*` / `post.*` on a rule entry.
    pub fn load(path: &Path) -> Result<Self, WorkflowError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(WorkflowError::MissingFile {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(WorkflowError::Unreadable {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        let root: Value = toml::from_str(&raw).map_err(|source| {
            WorkflowError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })?;

        let admission = parse_admission(path, &root)?;
        let repo = parse_first_repo(path, &root)?;
        let rules = parse_rules(path, &root)?;

        Ok(WorkflowConfig {
            admission,
            repo,
            rules,
        })
    }
}

// ---------- Validators ----------

fn parse_admission(
    path: &Path,
    root: &Value,
) -> Result<AdmissionSection, WorkflowError> {
    let admission_table = root
        .get("admission")
        .and_then(Value::as_table)
        .ok_or_else(|| WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: "admission.assignee".to_string(),
        })?;

    let assignee = admission_table
        .get("assignee")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: "admission.assignee".to_string(),
        })?
        .to_string();

    Ok(AdmissionSection { assignee })
}

/// Take the first `[[admission.repos]]` entry's `ghq`. Tolerates absence
/// of the array (returns `None`) and ignores per-entry `when.*` and
/// `workflow` override fields per Req 2.5.
fn parse_first_repo(
    path: &Path,
    root: &Value,
) -> Result<Option<AdmissionRepo>, WorkflowError> {
    // `[[admission.repos]]` is parsed as `admission.repos = [..]`.
    let Some(repos_value) = root
        .get("admission")
        .and_then(Value::as_table)
        .and_then(|t| t.get("repos"))
    else {
        return Ok(None);
    };

    let repos = repos_value.as_array().ok_or_else(|| {
        WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: "admission.repos".to_string(),
        }
    })?;

    let Some(first) = repos.first() else {
        return Ok(None);
    };

    let table = first.as_table().ok_or_else(|| {
        WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: "admission.repos[0]".to_string(),
        }
    })?;

    let ghq = table
        .get("ghq")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: "admission.repos[0].ghq".to_string(),
        })?
        .to_string();

    Ok(Some(AdmissionRepo { ghq }))
}

fn parse_rules(path: &Path, root: &Value) -> Result<Vec<Rule>, WorkflowError> {
    let Some(rule_value) = root.get("rule") else {
        // No rules is not a load-time error; rule no-match is a runtime
        // info-log per Req 5.4.
        return Ok(Vec::new());
    };

    let raw_rules = rule_value.as_array().ok_or_else(|| {
        WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: "rule".to_string(),
        }
    })?;

    let mut rules = Vec::with_capacity(raw_rules.len());
    for (idx, entry) in raw_rules.iter().enumerate() {
        rules.push(parse_rule_entry(path, idx, entry)?);
    }
    Ok(rules)
}

fn parse_rule_entry(
    path: &Path,
    idx: usize,
    entry: &Value,
) -> Result<Rule, WorkflowError> {
    let table = entry.as_table().ok_or_else(|| {
        WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: format!("rule[{idx}]"),
        }
    })?;

    let when = parse_when(path, idx, table)?;

    // run is required, pre and post are optional.
    let run = table
        .get("run")
        .ok_or_else(|| WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].run"),
        })
        .and_then(|val| parse_phase_body(path, &format!("rule[{idx}].run"), val))?;

    let pre = match table.get("pre") {
        Some(val) => Some(parse_phase_body(path, &format!("rule[{idx}].pre"), val)?),
        None => None,
    };
    let post = match table.get("post") {
        Some(val) => Some(parse_phase_body(path, &format!("rule[{idx}].post"), val)?),
        None => None,
    };

    Ok(Rule {
        when_status: when.status,
        when_labels_has_all: when.labels_has_all,
        pre,
        run,
        post,
    })
}

struct WhenClause {
    status: String,
    labels_has_all: Vec<String>,
}

fn parse_when(
    path: &Path,
    idx: usize,
    rule_table: &toml::map::Map<String, Value>,
) -> Result<WhenClause, WorkflowError> {
    let when_value = rule_table.get("when").ok_or_else(|| {
        WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].when"),
        }
    })?;
    let when_table = when_value.as_table().ok_or_else(|| {
        WorkflowError::UnsupportedWhen {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].when"),
        }
    })?;

    // Strict allow-list: only `status` and `labels` keys are permitted.
    // `labels` may carry only `has_all` per Req 5.2 / 5.3.
    for key in when_table.keys() {
        match key.as_str() {
            "status" | "labels" => {}
            other => {
                return Err(WorkflowError::UnsupportedWhen {
                    path: path.to_path_buf(),
                    key: format!("rule[{idx}].when.{other}"),
                });
            }
        }
    }

    let status = when_table
        .get("status")
        .ok_or_else(|| WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].when.status"),
        })?
        .as_str()
        .ok_or_else(|| WorkflowError::UnsupportedWhen {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].when.status"),
        })?
        .to_string();

    let labels_has_all = parse_when_labels(path, idx, when_table)?;

    Ok(WhenClause {
        status,
        labels_has_all,
    })
}

fn parse_when_labels(
    path: &Path,
    idx: usize,
    when_table: &toml::map::Map<String, Value>,
) -> Result<Vec<String>, WorkflowError> {
    let labels_value =
        when_table
            .get("labels")
            .ok_or_else(|| WorkflowError::MissingField {
                path: path.to_path_buf(),
                key: format!("rule[{idx}].when.labels.has_all"),
            })?;
    let labels_table = labels_value.as_table().ok_or_else(|| {
        WorkflowError::UnsupportedWhen {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].when.labels"),
        }
    })?;

    // Strict allow-list inside `when.labels`: only `has_all` is supported.
    for key in labels_table.keys() {
        if key != "has_all" {
            return Err(WorkflowError::UnsupportedWhen {
                path: path.to_path_buf(),
                key: format!("rule[{idx}].when.labels.{key}"),
            });
        }
    }

    let has_all_value = labels_table.get("has_all").ok_or_else(|| {
        WorkflowError::MissingField {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].when.labels.has_all"),
        }
    })?;
    let arr = has_all_value.as_array().ok_or_else(|| {
        WorkflowError::UnsupportedWhen {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].when.labels.has_all"),
        }
    })?;

    let mut labels = Vec::with_capacity(arr.len());
    for (label_idx, item) in arr.iter().enumerate() {
        let s = item.as_str().ok_or_else(|| {
            WorkflowError::UnsupportedWhen {
                path: path.to_path_buf(),
                key: format!("rule[{idx}].when.labels.has_all[{label_idx}]"),
            }
        })?;
        labels.push(s.to_string());
    }
    Ok(labels)
}

fn parse_phase_body(
    path: &Path,
    key_prefix: &str,
    value: &Value,
) -> Result<crate::engine::outcome::PhaseBody, WorkflowError> {
    let table = value.as_table().ok_or_else(|| {
        WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: key_prefix.to_string(),
        }
    })?;

    // session = "session" is recognised but not implemented in slice 1.
    if let Some(session_val) = table.get("session") {
        let kind = session_val.as_str().unwrap_or("");
        return Err(WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: format!("{key_prefix}.session={kind}"),
        });
    }

    // Allow-list of recognised phase-body keys.
    for key in table.keys() {
        match key.as_str() {
            "cmd" | "prompt" | "path" | "cli" => {}
            other => {
                return Err(WorkflowError::UnsupportedRunForm {
                    path: path.to_path_buf(),
                    key: format!("{key_prefix}.{other}"),
                });
            }
        }
    }

    let has_cmd = table.contains_key("cmd");
    let has_prompt = table.contains_key("prompt");
    let has_path = table.contains_key("path");

    let count = [has_cmd, has_prompt, has_path]
        .iter()
        .filter(|present| **present)
        .count();
    if count != 1 {
        return Err(WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: key_prefix.to_string(),
        });
    }

    if has_cmd {
        let cmd = table
            .get("cmd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| WorkflowError::UnsupportedRunForm {
                path: path.to_path_buf(),
                key: format!("{key_prefix}.cmd"),
            })?
            .to_string();
        Ok(crate::engine::outcome::PhaseBody::InlineCmd { cmd })
    } else if has_prompt {
        let prompt = table
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| WorkflowError::UnsupportedRunForm {
                path: path.to_path_buf(),
                key: format!("{key_prefix}.prompt"),
            })?
            .to_string();
        Ok(crate::engine::outcome::PhaseBody::InlinePrompt { prompt })
    } else {
        let path_str = table
            .get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| WorkflowError::UnsupportedRunForm {
                path: path.to_path_buf(),
                key: format!("{key_prefix}.path"),
            })?
            .to_string();
        let cli_override = table.get("cli").and_then(Value::as_str).map(str::to_string);
        // Path variant currently stores the path string in `body`. Task 10
        // executor reads the file at launch and renders the post-frontmatter
        // body.
        Ok(crate::engine::outcome::PhaseBody::Path {
            body: path_str,
            cli_override,
        })
    }
}

// `_path` parameter naming is intentional on the helper that does not need
// the path: keeping a uniform helper signature aids future per-key error
// uplift without touching call sites.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_toml(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("WORKFLOW.toml");
        let mut f = std::fs::File::create(&path).expect("create toml");
        f.write_all(body.as_bytes()).expect("write toml");
        path
    }

    const HAPPY_PATH_TOML: &str = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "In Progress"
[rule.when.labels]
has_all = ["needs-impl"]
[rule.run]
cmd = "echo hello"
"#;

    #[test]
    fn happy_path_loads_admission_repo_and_rule() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, HAPPY_PATH_TOML);

        let cfg = WorkflowConfig::load(&path).expect("happy path should load");

        assert_eq!(cfg.admission.assignee, "me");
        let repo = cfg.repo.as_ref().expect("first repo present");
        assert_eq!(repo.ghq, "github.com/acme/widget");
        assert_eq!(cfg.rules.len(), 1);
        let rule = &cfg.rules[0];
        assert_eq!(rule.when_status, "In Progress");
        assert_eq!(rule.when_labels_has_all, vec!["needs-impl".to_string()]);
        match &rule.run {
            crate::engine::outcome::PhaseBody::InlineCmd { cmd } => assert_eq!(cmd, "echo hello"),
            other => panic!("expected InlineCmd, got {other:?}"),
        }
        assert!(rule.pre.is_none());
        assert!(rule.post.is_none());
    }

    #[test]
    fn rejects_when_assignee_with_key_path() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "In Progress"
assignee = "me"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "echo hi"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path)
            .expect_err("when.assignee must be rejected");
        match err {
            WorkflowError::UnsupportedWhen { key, .. } => {
                assert!(
                    key.contains("assignee"),
                    "key path must mention assignee: {key}"
                );
                assert!(key.starts_with("rule[0]"), "key path: {key}");
            }
            other => panic!("expected UnsupportedWhen, got {other:?}"),
        }
    }

    #[test]
    fn rejects_rule_missing_run_with_key_path() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "In Progress"
[rule.when.labels]
has_all = []
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path)
            .expect_err("missing run table must be rejected");
        match err {
            WorkflowError::UnsupportedRunForm { key, .. } => {
                assert!(key.contains("run"), "key path: {key}");
                assert!(key.starts_with("rule[0]"), "key path: {key}");
            }
            other => panic!("expected UnsupportedRunForm, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_block_present_loads_ok() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "In Progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "echo hi"

[[cleanup]]
cmd = "git status"
"#;
        let path = write_toml(&dir, body);

        let cfg = WorkflowConfig::load(&path)
            .expect("cleanup presence must be tolerated");
        assert_eq!(cfg.rules.len(), 1);
    }

    #[test]
    fn missing_admission_repos_loads_with_repo_none() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[rule]]
[rule.when]
status = "In Progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "echo hi"
"#;
        let path = write_toml(&dir, body);

        let cfg = WorkflowConfig::load(&path).expect("loads ok with no repos");
        assert!(cfg.repo.is_none());
    }

    #[test]
    fn first_repo_workflow_override_is_tolerated_and_ghq_taken() {
        let dir = tempfile::tempdir().unwrap();
        // Per-repo `when.*` and `workflow` overrides must not abort the load
        // (Req 2.5). They are not consumed at the skeleton layer.
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"
workflow = "WORKFLOW.widget.toml"
[admission.repos.when]
repo = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "In Progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "echo hi"
"#;
        let path = write_toml(&dir, body);

        let cfg = WorkflowConfig::load(&path)
            .expect("per-repo overrides must be tolerated");
        let repo = cfg.repo.as_ref().expect("first repo present");
        assert_eq!(repo.ghq, "github.com/acme/widget");
    }

    #[test]
    fn rejects_when_labels_has_any() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "In Progress"
[rule.when.labels]
has_all = []
has_any = ["other"]
[rule.run]
cmd = "echo hi"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path)
            .expect_err("has_any must be rejected");
        match err {
            WorkflowError::UnsupportedWhen { key, .. } => {
                assert!(
                    key.contains("labels.has_any"),
                    "key path must mention has_any: {key}"
                );
            }
            other => panic!("expected UnsupportedWhen, got {other:?}"),
        }
    }

    #[test]
    fn rejects_run_with_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "echo hi"
foo = "bar"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path).expect_err("unknown run key rejected");
        match err {
            WorkflowError::UnsupportedRunForm { key, .. } => {
                assert!(key.contains("foo"), "key path: {key}");
            }
            other => panic!("expected UnsupportedRunForm, got {other:?}"),
        }
    }

    #[test]
    fn accepts_pre_run_post_inline_cmds() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.pre]
cmd = "echo pre"
[rule.run]
cmd = "echo run"
[rule.post]
cmd = "echo post"
"#;
        let path = write_toml(&dir, body);

        let cfg = WorkflowConfig::load(&path).expect("loads ok");
        let rule = &cfg.rules[0];
        match &rule.pre {
            Some(crate::engine::outcome::PhaseBody::InlineCmd { cmd }) => assert_eq!(cmd, "echo pre"),
            other => panic!("expected pre InlineCmd, got {other:?}"),
        }
        match &rule.run {
            crate::engine::outcome::PhaseBody::InlineCmd { cmd } => assert_eq!(cmd, "echo run"),
            other => panic!("expected run InlineCmd, got {other:?}"),
        }
        match &rule.post {
            Some(crate::engine::outcome::PhaseBody::InlineCmd { cmd }) => assert_eq!(cmd, "echo post"),
            other => panic!("expected post InlineCmd, got {other:?}"),
        }
    }

    #[test]
    fn accepts_inline_prompt_form() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.pre]
prompt = "decide what to do"
[rule.run]
cmd = "echo run"
"#;
        let path = write_toml(&dir, body);

        let cfg = WorkflowConfig::load(&path).expect("loads ok");
        match &cfg.rules[0].pre {
            Some(crate::engine::outcome::PhaseBody::InlinePrompt { prompt }) => {
                assert_eq!(prompt, "decide what to do");
            }
            other => panic!("expected pre InlinePrompt, got {other:?}"),
        }
    }

    #[test]
    fn rejects_session_shape() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.pre]
session = "session"
prompt = "x"
[rule.run]
cmd = "echo run"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path).expect_err("session shape rejected");
        match err {
            WorkflowError::UnsupportedRunForm { key, .. } => {
                assert!(key.contains("session"), "key path: {key}");
            }
            other => panic!("expected UnsupportedRunForm, got {other:?}"),
        }
    }

    #[test]
    fn rejects_run_with_both_cmd_and_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "echo a"
prompt = "do x"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path).expect_err("both cmd+prompt is ambiguous");
        match err {
            WorkflowError::UnsupportedRunForm { .. } => {}
            other => panic!("expected UnsupportedRunForm, got {other:?}"),
        }
    }

    #[test]
    fn missing_admission_assignee_returns_missing_field() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
# assignee missing on purpose

[[admission.repos]]
ghq = "github.com/acme/widget"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path)
            .expect_err("missing admission.assignee fails");
        match err {
            WorkflowError::MissingField { key, .. } => {
                assert_eq!(key, "admission.assignee");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_returns_missing_file_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let err = WorkflowConfig::load(&missing)
            .expect_err("missing file fails");
        match err {
            WorkflowError::MissingFile { path } => {
                assert_eq!(path, missing);
            }
            other => panic!("expected MissingFile, got {other:?}"),
        }
    }
}
