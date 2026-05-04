//! Allowlist matching for [`crate::worktree_manager::WorktreeManager`].
//!
//! Spec refs: requirements.md Req 4.5, 10.1.

use crate::config::repos::RepoEntry;
use crate::tracker::model::RepoId;

/// `true` iff `repo_id` exactly matches one of the configured `[[repos]]`
/// entries by ghq identifier.
pub fn is_allowed(allowlist: &[RepoEntry], repo_id: &RepoId) -> bool {
    allowlist.iter().any(|entry| entry.ghq == repo_id.0)
}
