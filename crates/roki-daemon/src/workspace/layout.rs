//! Workspace path derivation and identifier sanitization.
//!
//! This module is the pure half of the workspace boundary: given a
//! `workspace_root`, a `RepoId`, and an `IssueId`, it derives the workspace
//! path, sanitizes the identifier components, and enforces the path-safety
//! invariants required by Requirement 4.2 (no escape, no traversal, no
//! collisions). All filesystem effects live in `super::manager`; this module
//! never touches the filesystem itself.
//!
//! ## Sanitization rules (Requirement 4.2)
//!
//! 1. Replace any character outside `[A-Za-z0-9._-]` with `_`.
//! 2. Reject identifiers that are empty after sanitization.
//! 3. Reject identifiers consisting solely of `.` or `..` after sanitization
//!    (path traversal sentinels).
//! 4. Reject raw identifiers that contain `/` or `\` so a caller cannot smuggle
//!    a path component through sanitization (the rule rejects the identifier
//!    rather than silently splitting it).
//! 5. The caller is responsible for collision detection across distinct raw
//!    identifiers that sanitize to the same value; see `WorkspaceManager` for
//!    that bookkeeping.

/// Reasons an identifier is rejected before it ever reaches the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SanitizeReject {
    /// The raw identifier was empty.
    Empty,
    /// The raw identifier contained a path separator (`/` or `\`).
    ContainsPathSeparator,
    /// After sanitization the identifier was empty (e.g. all characters were
    /// replaced and then trimmed).
    EmptyAfterSanitization,
    /// After sanitization the identifier collapsed to `.` or `..`.
    PathTraversalSentinel,
}

impl SanitizeReject {
    pub(super) fn describe(&self) -> &'static str {
        match self {
            SanitizeReject::Empty => "identifier is empty",
            SanitizeReject::ContainsPathSeparator => {
                "identifier contains a path separator ('/' or '\\\\')"
            }
            SanitizeReject::EmptyAfterSanitization => "identifier is empty after sanitization",
            SanitizeReject::PathTraversalSentinel => {
                "sanitized identifier is a path traversal sentinel ('.' or '..')"
            }
        }
    }
}

/// Sanitize a single path component.
///
/// Returns `Ok(sanitized)` on success or `Err(reason)` if the identifier is
/// rejected by Requirement 4.2.
pub(super) fn sanitize_component(raw: &str) -> Result<String, SanitizeReject> {
    if raw.is_empty() {
        return Err(SanitizeReject::Empty);
    }
    if raw.contains('/') || raw.contains('\\') {
        return Err(SanitizeReject::ContainsPathSeparator);
    }
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        return Err(SanitizeReject::EmptyAfterSanitization);
    }
    if sanitized == "." || sanitized == ".." {
        return Err(SanitizeReject::PathTraversalSentinel);
    }
    Ok(sanitized)
}

/// Whether a sanitization rejection came from the repo or issue component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ComponentKind {
    Repo,
    Issue,
}

impl ComponentKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            ComponentKind::Repo => "repo",
            ComponentKind::Issue => "issue",
        }
    }
}

/// A failed sanitization carries the raw input, which component it came from,
/// and the rejection reason. The manager turns this into a `WorkspaceError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SanitizeRejection {
    pub(super) which: ComponentKind,
    pub(super) raw: String,
    pub(super) reason: SanitizeReject,
}

impl SanitizeRejection {
    pub(super) fn message(&self) -> String {
        format!(
            "{} identifier '{}' rejected: {}",
            self.which.label(),
            self.raw,
            self.reason.describe(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_passes_allowed_characters_through_unchanged() {
        let result = sanitize_component("abc.DEF_123-xyz").unwrap();
        assert_eq!(result, "abc.DEF_123-xyz");
    }

    #[test]
    fn sanitize_replaces_disallowed_characters_with_underscore() {
        let result = sanitize_component("abc def!ghi").unwrap();
        assert_eq!(result, "abc_def_ghi");
    }

    #[test]
    fn sanitize_rejects_empty_input() {
        assert_eq!(sanitize_component(""), Err(SanitizeReject::Empty));
    }

    #[test]
    fn sanitize_rejects_forward_slash() {
        assert_eq!(
            sanitize_component("abc/def"),
            Err(SanitizeReject::ContainsPathSeparator),
        );
    }

    #[test]
    fn sanitize_rejects_backslash() {
        assert_eq!(
            sanitize_component("abc\\def"),
            Err(SanitizeReject::ContainsPathSeparator),
        );
    }

    #[test]
    fn sanitize_rejects_dot() {
        assert_eq!(
            sanitize_component("."),
            Err(SanitizeReject::PathTraversalSentinel),
        );
    }

    #[test]
    fn sanitize_rejects_double_dot() {
        assert_eq!(
            sanitize_component(".."),
            Err(SanitizeReject::PathTraversalSentinel),
        );
    }
}
