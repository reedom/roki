//! Per-repository configuration entries.
//!
//! Post task 7.1a, the per-repo block is a pure allowlist: each entry carries
//! only the `ghq` identifier the agent is permitted to open a worktree in via
//! the `roki_open_worktree` tool. The Linear scope, per-repo webhook secret,
//! per-repo `WORKFLOW.md`, and operator-supplied `id` fields were removed
//! along with the daemon-side routing logic — the agent reads each ticket on
//! its first turn and decides which repo(s) to operate in.
//!
//! ## Worktree migration (task 6.1)
//!
//! Per `.kiro/specs/roki-mvp/design-worktree-workspace.md`, the per-repo
//! configuration carries a `repo` ghq identifier (`owner/repo` or
//! `host/owner/repo`). The local checkout is resolved at runtime via
//! `ghq list -p` / `ghq get`, and workspaces are git worktrees laid out by
//! `wt`.

use serde::{Deserialize, Serialize};

/// Configuration for a single Git repository served by the daemon.
///
/// Post task 7.1a, this is the agent allowlist — no per-repo scope or
/// secret. Requirement 2.1: each entry declares its `ghq` identifier and the
/// daemon resolves the local clone path at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Ghq identifier for the repo's source remote
    /// (`owner/repo` or `host/owner/repo`). The daemon uses this to discover
    /// the local checkout via `ghq list -p`, cloning on miss via `ghq get`.
    /// Validated at config load (see [`validate_ghq_identifier`]).
    pub repo: String,
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
