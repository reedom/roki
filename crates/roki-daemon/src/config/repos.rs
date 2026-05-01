//! Per-repository configuration entries.
//!
//! This module owns the shape of the per-repo block in the daemon config. The
//! deterministic routing precedence rule across overlapping Linear scopes is
//! explicitly NOT implemented here — that lives in task 1.5. We only define
//! the data so task 1.2's loader can validate it.
//!
//! ## Worktree migration (task 6.1)
//!
//! Per `.kiro/specs/roki-mvp/design-worktree-workspace.md`, the per-repo
//! configuration carries a `repo` ghq identifier (`owner/repo` or
//! `host/owner/repo`) instead of an absolute working-tree `path`. The local
//! checkout is resolved at runtime via `ghq list -p` / `ghq get`, and
//! workspaces are git worktrees laid out by `wt`. The schema rename here is
//! the operator-visible half of that migration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration for a single Git repository served by the daemon.
///
/// Requirement 2.1: each repo declares its own ghq identifier, Linear team or
/// label scope, and `WORKFLOW.md` location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Stable identifier for this repo (used as the `repo` half of the
    /// `(repo, issue)` workspace key).
    pub id: String,

    /// Ghq identifier for the repo's source remote
    /// (`owner/repo` or `host/owner/repo`). The daemon uses this to discover
    /// the local checkout via `ghq list -p`, cloning on miss via `ghq get`.
    /// Validated at config load (see [`validate_ghq_identifier`]).
    pub repo: String,

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

/// Result of validating a ghq identifier candidate. Keeps the caller's error
/// type free of this module's concerns — the loader translates rejections
/// into a `ConfigError::InvalidField` whose `field` names the offending repo.
#[derive(Debug, PartialEq, Eq)]
pub enum GhqIdentifierError {
    /// The identifier was empty or whitespace-only.
    Empty,
    /// The identifier contained whitespace, which `ghq` does not accept.
    ContainsWhitespace,
    /// The identifier started with `/`, indicating a smuggled absolute path.
    LeadingSlash,
    /// The identifier contained a `..` segment, which would let a caller
    /// escape `ghq`'s root via path traversal.
    PathTraversal,
    /// The identifier did not match the documented `<token>/<token>` or
    /// `<host>/<token>/<token>` shape.
    Shape,
}

impl GhqIdentifierError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Empty => "ghq identifier must not be empty",
            Self::ContainsWhitespace => "ghq identifier must not contain whitespace",
            Self::LeadingSlash => "ghq identifier must not start with '/'",
            Self::PathTraversal => "ghq identifier must not contain '..' segments",
            Self::Shape => "ghq identifier must match `<owner>/<repo>` or `<host>/<owner>/<repo>`",
        }
    }
}

/// Validate a ghq identifier candidate.
///
/// Accepts `<token>/<token>` (e.g., `owner/repo`) or
/// `<host>/<token>/<token>` (e.g., `github.com/owner/repo`). Tokens must be
/// non-empty and free of whitespace, leading `/`, and `..` segments. Internal
/// characters are intentionally permissive (matching ghq's acceptance) so the
/// daemon does not over-reject identifiers `ghq` itself would resolve.
pub fn validate_ghq_identifier(raw: &str) -> Result<(), GhqIdentifierError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(GhqIdentifierError::Empty);
    }
    if raw.chars().any(|c| c.is_whitespace()) {
        return Err(GhqIdentifierError::ContainsWhitespace);
    }
    if raw.starts_with('/') {
        return Err(GhqIdentifierError::LeadingSlash);
    }
    let segments: Vec<&str> = raw.split('/').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(GhqIdentifierError::Shape);
    }
    if segments.iter().any(|s| *s == ".." || *s == ".") {
        return Err(GhqIdentifierError::PathTraversal);
    }
    if segments.len() != 2 && segments.len() != 3 {
        return Err(GhqIdentifierError::Shape);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_owner_slash_repo() {
        assert!(validate_ghq_identifier("owner/repo").is_ok());
    }

    #[test]
    fn accepts_host_slash_owner_slash_repo() {
        assert!(validate_ghq_identifier("github.com/owner/repo").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(validate_ghq_identifier(""), Err(GhqIdentifierError::Empty),);
    }

    #[test]
    fn rejects_whitespace() {
        assert_eq!(
            validate_ghq_identifier("owner/ repo"),
            Err(GhqIdentifierError::ContainsWhitespace),
        );
    }

    #[test]
    fn rejects_leading_slash() {
        assert_eq!(
            validate_ghq_identifier("/owner/repo"),
            Err(GhqIdentifierError::LeadingSlash),
        );
    }

    #[test]
    fn rejects_path_traversal() {
        assert_eq!(
            validate_ghq_identifier("owner/../etc"),
            Err(GhqIdentifierError::PathTraversal),
        );
    }

    #[test]
    fn rejects_single_token() {
        assert_eq!(
            validate_ghq_identifier("repo"),
            Err(GhqIdentifierError::Shape),
        );
    }

    #[test]
    fn rejects_too_many_segments() {
        assert_eq!(
            validate_ghq_identifier("a/b/c/d"),
            Err(GhqIdentifierError::Shape),
        );
    }

    #[test]
    fn rejects_empty_segment() {
        assert_eq!(
            validate_ghq_identifier("owner//repo"),
            Err(GhqIdentifierError::Shape),
        );
    }
}
