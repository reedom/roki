//! YAML deserializer + per-repo override loader + path resolution.
//!
//! Spec: §3.1, §3.2, §3.2.1, §3.3 (sugar form acceptance).
//!
//! Output is a `RawWorkflow` IR carrying both Sugar and Canonical rule bodies.
//! The 5-pass sugar→canonical expansion lives in `workflow::sugar`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use super::canonical::{DirectiveName, StateId};

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("read error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("yaml deserialize error in {path}: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml_ng::Error,
    },
    #[error("empty body not allowed outside cleanup: {path}: {section}[{index}]")]
    EmptyBodyOutsideCleanup {
        path: PathBuf,
        section: &'static str,
        index: usize,
    },
    #[error("symlink escape detected resolving {declared} from {reference}")]
    SymlinkEscape {
        reference: PathBuf,
        declared: PathBuf,
    },
    #[error("home directory not resolvable for tilde-prefixed path: {0}")]
    HomeUnresolvable(PathBuf),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawWorkflow {
    pub admission: Option<RawAdmission>,
    #[serde(default)]
    pub rules: Vec<RawRuleEntry>,
    #[serde(default)]
    pub cleanup: Vec<RawRuleEntry>,
    #[serde(default)]
    pub on_failure: Vec<RawRuleEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawAdmission {
    pub assignee: String,
    pub repos: Vec<RawRepoEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawRepoEntry {
    pub ghq: String,
    #[serde(default)]
    pub when: Option<RawWhenClause>,
    #[serde(default)]
    pub workflow: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawWhenClause {
    pub status: Option<RawScalarMatcher>,
    pub labels: Option<RawLabelsMatcher>,
    pub assignee: Option<RawScalarMatcher>,
    pub repo: Option<String>,
    pub kind: Option<RawScalarMatcher>,
    pub phase: Option<RawScalarMatcher>,
    pub title: Option<RawTextMatcher>,
    pub body: Option<RawTextMatcher>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawScalarMatcher {
    /// Bare scalar — equality match.
    Eq(String),
    /// Map form — `not:` / `in:` operator.
    Op {
        #[serde(default)]
        not: Option<String>,
        #[serde(default, rename = "in")]
        in_: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawLabelsMatcher {
    pub has_all: Option<Vec<String>>,
    pub has_any: Option<Vec<String>>,
    pub has_none: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawTextMatcher {
    pub regex: Option<String>,
    pub starts_with: Option<String>,
    pub contains: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawRuleEntry {
    #[serde(default)]
    pub when: Option<RawWhenClause>,
    #[serde(flatten)]
    pub body: RawRuleBody,
}

/// Untagged: serde tries Canonical first (fields `start` + `states`), then
/// Sugar (`tasks:`), then Empty (no body keys). Sugar+Canonical mixing on the
/// same entry is rejected because each variant declares its discriminator.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum RawRuleBody {
    Canonical {
        start: StateId,
        states: BTreeMap<StateId, RawStateEntry>,
        terminals: BTreeMap<StateId, RawTerminalEntry>,
        #[serde(default)]
        on_fail: Option<StateId>,
    },
    Sugar {
        tasks: Vec<RawTaskEntry>,
        #[serde(default)]
        states: BTreeMap<StateId, RawStateEntry>,
        #[serde(default)]
        terminals: BTreeMap<StateId, RawTerminalEntry>,
        #[serde(default)]
        on_fail: Option<StateId>,
    },
    Empty {},
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawStateEntry {
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub uses: Option<PathBuf>,
    #[serde(default, rename = "if")]
    pub if_cond: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub on_done: Option<StateId>,
    #[serde(default)]
    pub on_fail: Option<StateId>,
    #[serde(default)]
    pub directives: BTreeMap<DirectiveName, RawDirectiveTarget>,
    #[serde(default)]
    pub max_visits: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawDirectiveTarget {
    /// Short form: `<name>: <state_id>`.
    Short(StateId),
    /// Long form: `<name>: { target: <state_id>, max_visits: <int> }`.
    Long {
        target: StateId,
        #[serde(default)]
        max_visits: Option<u32>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawTaskEntry {
    pub id: StateId,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub uses: Option<PathBuf>,
    #[serde(default, rename = "if")]
    pub if_cond: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub on_fail: Option<StateId>,
    #[serde(default)]
    pub directives: BTreeMap<DirectiveName, RawDirectiveTarget>,
    #[serde(default)]
    pub max_visits: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawTerminalEntry {
    pub outcome: String,
}

/// Read a YAML workflow file from disk + deserialize. Does not perform sugar
/// expansion; that is `workflow::sugar::expand`'s job.
pub fn parse_workflow_file(path: &Path) -> Result<RawWorkflow, ParseError> {
    let text = fs::read_to_string(path).map_err(|source| ParseError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_workflow_str(path, &text)
}

/// Deserialize an in-memory YAML string. `path` is used for error reporting.
pub fn parse_workflow_str(path: &Path, text: &str) -> Result<RawWorkflow, ParseError> {
    let raw: RawWorkflow = serde_yaml_ng::from_str(text).map_err(|source| ParseError::Yaml {
        path: path.to_path_buf(),
        source,
    })?;
    reject_empty_body_outside_cleanup(path, &raw)?;
    Ok(raw)
}

fn reject_empty_body_outside_cleanup(path: &Path, raw: &RawWorkflow) -> Result<(), ParseError> {
    for (idx, rule) in raw.rules.iter().enumerate() {
        if matches!(rule.body, RawRuleBody::Empty {}) {
            return Err(ParseError::EmptyBodyOutsideCleanup {
                path: path.to_path_buf(),
                section: "rules",
                index: idx,
            });
        }
    }
    for (idx, rule) in raw.on_failure.iter().enumerate() {
        if matches!(rule.body, RawRuleBody::Empty {}) {
            return Err(ParseError::EmptyBodyOutsideCleanup {
                path: path.to_path_buf(),
                section: "on_failure",
                index: idx,
            });
        }
    }
    Ok(())
}

/// Resolve a declared path against its reference file.
///
/// Per spec §3.2.1:
/// - Absolute paths are returned as-is.
/// - Tilde-prefixed paths are home-expanded.
/// - Relative paths are resolved against `reference_file.parent()`.
///
/// Symlink escape detection is left to the caller: the daemon spawns the state
/// later and any escape surfaces as `fs_poison` at that time.
pub fn resolve_path(reference_file: &Path, declared: &Path) -> Result<PathBuf, ParseError> {
    if declared.is_absolute() {
        return Ok(declared.to_path_buf());
    }
    if let Some(stripped) = declared.to_str().and_then(|s| s.strip_prefix("~/")) {
        let home =
            dirs_home().ok_or_else(|| ParseError::HomeUnresolvable(declared.to_path_buf()))?;
        return Ok(home.join(stripped));
    }
    if declared.starts_with("~") && !declared.starts_with("~/") {
        // bare "~" or "~name" — only "~" alone is supported in this slice.
        let s = declared.to_str().unwrap_or("");
        if s == "~" {
            return dirs_home().ok_or_else(|| ParseError::HomeUnresolvable(declared.to_path_buf()));
        }
        // "~name" not supported.
        return Err(ParseError::HomeUnresolvable(declared.to_path_buf()));
    }
    let parent = reference_file.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(declared))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Walk every per-repo override declared in `top.admission.repos[].workflow`
/// and load each override file. Returns a map keyed by `ghq` → loaded
/// override `RawWorkflow`. Top-level admission stays in the caller's hands.
pub fn resolve_per_repo_overrides(
    top: &RawWorkflow,
    top_path: &Path,
) -> Result<BTreeMap<String, RawWorkflow>, ParseError> {
    let mut out = BTreeMap::new();
    let Some(admission) = &top.admission else {
        return Ok(out);
    };
    for repo in &admission.repos {
        let Some(override_path_decl) = &repo.workflow else {
            continue;
        };
        let resolved = resolve_path(top_path, override_path_decl)?;
        let raw = parse_workflow_file(&resolved)?;
        // Per-repo override files MUST NOT carry `admission:` — the contract
        // is "admission lives only in the top-level file". Validation of this
        // shape is delegated to `workflow::validate`.
        out.insert(repo.ghq.clone(), raw);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn minimal_sugar_round_trips() {
        let yaml = r#"
admission:
  assignee: me
  repos:
    - ghq: github.com/foo/bar
rules:
  - when:
      status: Todo
    tasks:
      - id: impl
        run: echo hi
"#;
        let raw = parse_workflow_str(Path::new("WORKFLOW.yaml"), yaml).unwrap();
        let admission = raw.admission.unwrap();
        assert_eq!(admission.assignee, "me");
        assert_eq!(admission.repos.len(), 1);
        assert_eq!(admission.repos[0].ghq, "github.com/foo/bar");
        assert_eq!(raw.rules.len(), 1);
        match &raw.rules[0].body {
            RawRuleBody::Sugar { tasks, .. } => {
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0].id, "impl");
                assert_eq!(tasks[0].run.as_deref(), Some("echo hi"));
            }
            other => panic!("expected Sugar body, got {other:?}"),
        }
    }

    #[test]
    fn full_canonical_round_trips() {
        let yaml = r#"
admission:
  assignee: me
  repos:
    - ghq: github.com/foo/bar
rules:
  - when:
      status: { in: [Todo, InProgress] }
      labels:
        has_all: [roki:ready]
        has_none: [roki:hold]
    start: judge
    states:
      judge:
        run: echo j
        on_done: impl
        on_fail: __failure__
        directives:
          skip: __no_action__
      impl:
        run: echo i
        on_done: __success__
        on_fail: __failure__
        directives:
          retry:
            target: impl
            max_visits: 5
    terminals:
      __success__: { outcome: success }
      __failure__: { outcome: failure }
      __no_action__: { outcome: no_action }
"#;
        let raw = parse_workflow_str(Path::new("WORKFLOW.yaml"), yaml).unwrap();
        match &raw.rules[0].body {
            RawRuleBody::Canonical {
                start,
                states,
                terminals,
                ..
            } => {
                assert_eq!(start, "judge");
                assert_eq!(states.len(), 2);
                assert_eq!(terminals.len(), 3);
                let impl_state = &states["impl"];
                let retry = &impl_state.directives["retry"];
                match retry {
                    RawDirectiveTarget::Long { target, max_visits } => {
                        assert_eq!(target, "impl");
                        assert_eq!(*max_visits, Some(5));
                    }
                    other => panic!("expected Long, got {other:?}"),
                }
            }
            other => panic!("expected Canonical body, got {other:?}"),
        }
        let when = raw.rules[0].when.as_ref().unwrap();
        match &when.status {
            Some(RawScalarMatcher::Op { in_: Some(v), .. }) => {
                assert_eq!(v, &vec!["Todo".to_string(), "InProgress".to_string()]);
            }
            other => panic!("expected status.in matcher, got {other:?}"),
        }
        assert_eq!(
            when.labels.as_ref().unwrap().has_all.as_ref().unwrap(),
            &vec!["roki:ready".to_string()]
        );
    }

    #[test]
    fn cleanup_immediate_delete_shorthand_accepts_empty_body() {
        let yaml = r#"
admission:
  assignee: me
  repos:
    - ghq: github.com/foo/bar
cleanup:
  - when:
      status: Done
"#;
        let raw = parse_workflow_str(Path::new("WORKFLOW.yaml"), yaml).unwrap();
        assert_eq!(raw.cleanup.len(), 1);
        match &raw.cleanup[0].body {
            RawRuleBody::Empty {} => {}
            other => panic!("expected Empty body, got {other:?}"),
        }
    }

    #[test]
    fn empty_body_in_rules_is_rejected() {
        let yaml = r#"
admission:
  assignee: me
  repos:
    - ghq: github.com/foo/bar
rules:
  - when:
      status: Todo
"#;
        let err = parse_workflow_str(Path::new("WORKFLOW.yaml"), yaml).unwrap_err();
        match err {
            ParseError::EmptyBodyOutsideCleanup { section, index, .. } => {
                assert_eq!(section, "rules");
                assert_eq!(index, 0);
            }
            other => panic!("expected EmptyBodyOutsideCleanup, got {other:?}"),
        }
    }

    #[test]
    fn empty_body_in_on_failure_is_rejected() {
        let yaml = r#"
on_failure:
  - when:
      kind: stall
"#;
        let err = parse_workflow_str(Path::new("WORKFLOW.yaml"), yaml).unwrap_err();
        match err {
            ParseError::EmptyBodyOutsideCleanup { section, .. } => {
                assert_eq!(section, "on_failure");
            }
            other => panic!("expected EmptyBodyOutsideCleanup, got {other:?}"),
        }
    }

    #[test]
    fn relative_path_resolves_against_reference_parent() {
        let p = resolve_path(
            Path::new("/tmp/repo/WORKFLOW.yaml"),
            Path::new("repos/bar.yaml"),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/tmp/repo/repos/bar.yaml"));
    }

    #[test]
    fn absolute_path_returned_unchanged() {
        let p = resolve_path(
            Path::new("/tmp/repo/WORKFLOW.yaml"),
            Path::new("/etc/foo.yaml"),
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/etc/foo.yaml"));
    }

    #[test]
    fn tilde_prefix_expands_to_home() {
        // Skip if HOME unset (CI sometimes).
        let Some(home) = dirs_home() else {
            return;
        };
        let p = resolve_path(
            Path::new("/tmp/repo/WORKFLOW.yaml"),
            Path::new("~/foo.yaml"),
        )
        .unwrap();
        assert_eq!(p, home.join("foo.yaml"));
    }

    #[test]
    fn per_repo_override_resolves_relative_to_top_file() {
        let dir = TempDir::new().unwrap();
        let top_yaml = r#"
admission:
  assignee: me
  repos:
    - ghq: github.com/foo/bar
      workflow: repos/bar.yaml
"#;
        let repo_yaml = r#"
rules:
  - when:
      status: Todo
    tasks:
      - id: impl
        run: echo bar
"#;
        let top_path = write(dir.path(), "WORKFLOW.yaml", top_yaml);
        write(dir.path(), "repos/bar.yaml", repo_yaml);

        let raw = parse_workflow_file(&top_path).unwrap();
        let overrides = resolve_per_repo_overrides(&raw, &top_path).unwrap();
        assert_eq!(overrides.len(), 1);
        let bar = &overrides["github.com/foo/bar"];
        assert_eq!(bar.rules.len(), 1);
    }

    #[test]
    fn malformed_yaml_returns_yaml_error() {
        let yaml = "rules:\n  - this is not a list item\n  garbage:::";
        let err = parse_workflow_str(Path::new("WORKFLOW.yaml"), yaml).unwrap_err();
        assert!(matches!(err, ParseError::Yaml { .. }));
    }
}
