//! Workflow loader: parses `WORKFLOW.yaml`, runs sugar→canonical expansion,
//! validates, and projects the result into the daemon-facing shape.
//!
//! Slice 8 replaced the legacy TOML loader (pre/run/post phases) with this
//! YAML-backed adapter. `crate::workflow::canonical` owns the canonical
//! types; this module is the runtime entry point.

#![allow(dead_code)]

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::WorkflowError;
use crate::workflow::canonical::{Admission, RuleEntry, WorkflowFile};
use crate::workflow::parse::{self, ParseError};
use crate::workflow::sugar::{self, ExpandConfig, ExpandError};

/// Top-level loaded workflow configuration.
#[derive(Clone, Debug)]
pub struct WorkflowConfig {
    pub admission: AdmissionSection,
    /// First `[[admission.repos]]` entry; `None` when the array is empty
    /// (admission surfaces `NoRepos` per Req 4.4). Per-repo override files
    /// are loaded into `repo_overrides` keyed by ghq.
    pub repo: Option<AdmissionRepo>,
    pub rules: Vec<RuleEntry>,
    pub cleanups: Vec<RuleEntry>,
    pub on_failures: Vec<RuleEntry>,
    /// Per-repo override workflows resolved at load time (one entry per
    /// `[[admission.repos]]` declaring `workflow:`).
    pub repo_overrides: std::collections::BTreeMap<String, Arc<RuleSet>>,
}

/// Carved-out portion of a `WorkflowFile` that the per-ticket dispatcher
/// applies (rules + cleanups + on_failures). Used when a per-repo override
/// replaces the top-level rule lists.
#[derive(Clone, Debug)]
pub struct RuleSet {
    pub rules: Vec<RuleEntry>,
    pub cleanups: Vec<RuleEntry>,
    pub on_failures: Vec<RuleEntry>,
}

/// `[admission]` section.
#[derive(Clone, Debug)]
pub struct AdmissionSection {
    pub assignee: String,
}

/// First `[[admission.repos]]` entry.
#[derive(Clone, Debug)]
pub struct AdmissionRepo {
    pub ghq: String,
}

impl WorkflowConfig {
    /// Load and validate the workflow file (YAML).
    pub fn load(path: &Path) -> Result<Self, WorkflowError> {
        Self::load_with(path, ExpandConfig::default())
    }

    /// Load with an explicit `ExpandConfig` (carries the `default_max_iterations`
    /// from `roki.toml [engine].max_iterations`).
    pub fn load_with(path: &Path, expand_config: ExpandConfig) -> Result<Self, WorkflowError> {
        let raw = match parse::parse_workflow_file(path) {
            Ok(r) => r,
            Err(ParseError::Io { path, source })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Err(WorkflowError::MissingFile { path });
            }
            Err(ParseError::Io { path, source }) => {
                return Err(WorkflowError::Unreadable { path, source });
            }
            Err(err) => {
                return Err(WorkflowError::YamlParse {
                    path: path.to_path_buf(),
                    detail: err.to_string(),
                });
            }
        };

        let overrides_raw = parse::resolve_per_repo_overrides(&raw, path).map_err(|e| {
            WorkflowError::YamlParse {
                path: path.to_path_buf(),
                detail: format!("per-repo override: {e}"),
            }
        })?;

        let file = match sugar::expand(raw, expand_config) {
            Ok(f) => f,
            Err(ExpandError::Validation(errors)) => {
                let detail = errors
                    .into_iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(WorkflowError::YamlValidation {
                    path: path.to_path_buf(),
                    detail,
                });
            }
            Err(other) => {
                return Err(WorkflowError::YamlValidation {
                    path: path.to_path_buf(),
                    detail: other.to_string(),
                });
            }
        };

        let admission = file
            .admission
            .as_ref()
            .ok_or_else(|| WorkflowError::MissingField {
                path: path.to_path_buf(),
                field: "admission".into(),
            })?;
        let admission_section = AdmissionSection {
            assignee: admission.assignee.clone(),
        };
        let repo = admission
            .repos
            .first()
            .map(|r| AdmissionRepo { ghq: r.ghq.clone() });

        let mut repo_overrides = std::collections::BTreeMap::new();
        for (ghq, raw_override) in overrides_raw {
            let expanded = match sugar::expand(raw_override, expand_config) {
                Ok(f) => f,
                Err(ExpandError::Validation(errors)) => {
                    let detail = errors
                        .into_iter()
                        .map(|e| e.to_string())
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(WorkflowError::YamlValidation {
                        path: path.to_path_buf(),
                        detail: format!("per-repo override for {ghq}: {detail}"),
                    });
                }
                Err(other) => {
                    return Err(WorkflowError::YamlValidation {
                        path: path.to_path_buf(),
                        detail: format!("per-repo override for {ghq}: {other}"),
                    });
                }
            };
            repo_overrides.insert(
                ghq,
                Arc::new(RuleSet {
                    rules: expanded.rules,
                    cleanups: expanded.cleanup,
                    on_failures: expanded.on_failure,
                }),
            );
        }

        Ok(WorkflowConfig {
            admission: admission_section,
            repo,
            rules: file.rules,
            cleanups: file.cleanup,
            on_failures: file.on_failure,
            repo_overrides,
        })
    }

    /// Produce a [`WorkflowFile`] view of this config. Used by code paths that
    /// already speak canonical types (e.g. the `roki workflow validate` CLI).
    pub fn to_workflow_file(&self) -> WorkflowFile {
        WorkflowFile {
            admission: Some(Admission {
                assignee: self.admission.assignee.clone(),
                repos: Vec::new(),
            }),
            rules: self.rules.clone(),
            cleanup: self.cleanups.clone(),
            on_failure: self.on_failures.clone(),
        }
    }
}

/// Test helper: build a `WorkflowConfig` directly from canonical pieces.
#[cfg(test)]
pub fn workflow_config_for_test(
    assignee: &str,
    repo: Option<&str>,
    rules: Vec<RuleEntry>,
    cleanups: Vec<RuleEntry>,
    on_failures: Vec<RuleEntry>,
) -> WorkflowConfig {
    WorkflowConfig {
        admission: AdmissionSection {
            assignee: assignee.into(),
        },
        repo: repo.map(|g| AdmissionRepo { ghq: g.into() }),
        rules,
        cleanups,
        on_failures,
        repo_overrides: std::collections::BTreeMap::new(),
    }
}

#[cfg(test)]
pub fn workflow_path_in(dir: &Path) -> PathBuf {
    dir.join("WORKFLOW.yaml")
}
