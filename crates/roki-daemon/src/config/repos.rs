//! Per-repository configuration entries.
//!
//! This module owns the shape of the per-repo block in the daemon config. The
//! deterministic routing precedence rule across overlapping Linear scopes is
//! explicitly NOT implemented here — that lives in task 1.5. We only define
//! the data so task 1.2's loader can validate it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration for a single Git repository served by the daemon.
///
/// Requirement 2.1: each repo declares its own local path, Linear team or
/// label scope, and `WORKFLOW.md` location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Stable identifier for this repo (used as the `repo` half of the
    /// `(repo, issue)` workspace key).
    pub id: String,

    /// Local filesystem path to the Git working tree.
    pub path: PathBuf,

    /// Linear scope this repo subscribes to.
    pub scope: LinearScope,

    /// Path to this repo's `WORKFLOW.md`, resolved relative to the daemon's
    /// working directory if not absolute.
    pub workflow_path: PathBuf,

    /// Environment variable holding the HMAC-SHA256 secret used to verify
    /// Linear webhook signatures for this repo. Preferred over the literal
    /// [`Self::webhook_secret`] form. Resolved at bootstrap; missing or empty
    /// values are a hard refusal (`runtime::run` errors). SPEC.md §3.2.
    #[serde(default)]
    pub webhook_secret_env: Option<String>,

    /// Literal HMAC-SHA256 webhook secret. Discouraged — set
    /// [`Self::webhook_secret_env`] instead so the secret never lands on disk.
    /// When this is used the bootstrap emits a WARN log on load.
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

/// Linear team or label scope a repository subscribes to.
///
/// Either form is accepted; downstream routing (task 1.5) must select exactly
/// one repository per `(repo, issue)` pair using a deterministic precedence
/// rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LinearScope {
    /// Match every issue from a Linear team identified by its key.
    Team { key: String },

    /// Match issues bearing any of the listed labels.
    Labels { any_of: Vec<String> },
}
