//! Multi-repo router and unhealthy-repo handling.
//!
//! This module implements task 1.5 of the roki-mvp spec. It owns:
//!
//! * the deterministic precedence rule for routing a Linear issue to exactly
//!   one configured repository when scopes overlap (Requirement 2.2);
//! * a startup health check that classifies each repository path as a Git
//!   working tree, missing, or non-Git, and refuses to schedule work for
//!   unhealthy entries while continuing to serve the rest (Requirement 2.3);
//! * the `(repo, issue)` keying contract that downstream specs depend on
//!   (Requirement 2.4) — the router emits exactly one such key per matched
//!   issue.
//!
//! Design notes:
//!
//! * The precedence rule is: a `LinearScope::Labels` match outranks a
//!   `LinearScope::Team` match because labels narrow the issue surface inside a
//!   team's issue stream (a label set is strictly more specific than a team
//!   key). When two repos match at the same specificity tier, the router
//!   tie-breaks by lexicographic order of `repo.id` so the decision is
//!   reproducible from the configuration alone. design.md does not pin a
//!   specific rule, only that one must exist and be logged; this rule is the
//!   simplest deterministic choice that respects scope semantics.
//! * The health check is filesystem-only (no `git` subprocess): a path is a
//!   Git working tree iff the path is an existing directory and contains a
//!   `.git` entry (either a directory or a `gitdir:` file pointing into a
//!   shared git directory). This keeps the daemon's startup path free of
//!   external commands and matches the design's "no external git binary"
//!   assumption.

use tracing::info;

use crate::config::repos::{LinearScope, RepoConfig};

/// Minimal projection of a Linear issue used for routing decisions.
///
/// The router needs the issue identifier (so it can return a `(repo, issue)`
/// key), the team key (to match `LinearScope::Team`), and the label set (to
/// match `LinearScope::Labels`). Anything else the tracker carries is
/// irrelevant here and intentionally excluded so the router stays decoupled
/// from the tracker's wire types.
#[derive(Debug, Clone)]
pub struct IssueRouteInput<'a> {
    /// Linear issue identifier (e.g., `ENG-42`).
    pub issue_id: &'a str,
    /// Team key the issue belongs to.
    pub team_key: &'a str,
    /// Labels currently attached to the issue.
    pub labels: &'a [String],
}

/// Result of a routing decision.
///
/// `Some((repo_id, issue_id))` names the single repository selected for the
/// issue. `None` means no configured scope matched; the caller drops the
/// issue. Both outcomes are logged.
pub type RouteOutcome = Option<(String, String)>;

/// Specificity tier for a scope match. Higher tiers win.
///
/// `Labels` outranks `Team`: a label-matched scope is strictly narrower than
/// a team-matched scope because labels filter inside a team's issue stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Specificity {
    /// The repo's `LinearScope::Team` matched the issue's team key.
    Team,
    /// The repo's `LinearScope::Labels` matched at least one of the issue's
    /// labels.
    Labels,
}

impl Specificity {
    fn name(self) -> &'static str {
        match self {
            Self::Team => "team_match",
            Self::Labels => "labels_match",
        }
    }
}

/// Reason a repository was marked unhealthy at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnhealthyReason {
    /// The configured ghq identifier was malformed (empty, contained
    /// whitespace, etc.). Pre-task-6.1 this also covered "path does not
    /// exist", which is now a runtime concern delegated to `ghq`.
    InvalidGhqIdentifier,
}

impl UnhealthyReason {
    fn name(&self) -> &'static str {
        match self {
            Self::InvalidGhqIdentifier => "invalid_ghq_identifier",
        }
    }
}

/// A repository whose configured ghq identifier failed the startup health
/// check. With the task-6.1 worktree model, "path" here refers to the ghq
/// identifier the operator configured (`owner/repo` or `host/owner/repo`);
/// the actual filesystem path is resolved at runtime via `ghq` and is not
/// known at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnhealthyRepo {
    pub repo_id: String,
    pub identifier: String,
    pub reason: UnhealthyReason,
}

/// Outcome of the startup health check.
///
/// The `healthy` list is what the orchestrator schedules work against; the
/// `unhealthy` list is reported in logs and held aside until the operator
/// intervenes (Requirement 2.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoHealth {
    pub healthy: Vec<RepoConfig>,
    pub unhealthy: Vec<UnhealthyRepo>,
}

/// Inspect every configured repository and split it into healthy and
/// unhealthy buckets.
///
/// Pre-task-6.1 this performed a filesystem health check against the
/// repo's `path`. The worktree model resolves the local checkout at
/// runtime via `ghq`, so the only startup-level check that remains is the
/// shape of the configured ghq identifier. Runtime clone / network
/// failures are surfaced per-issue by the workspace adapter.
pub fn classify_repo_health(repos: &[RepoConfig]) -> RepoHealth {
    let mut healthy = Vec::with_capacity(repos.len());
    let mut unhealthy = Vec::new();

    for repo in repos {
        match crate::config::validate_ghq_identifier(&repo.repo) {
            Ok(()) => {
                info!(
                    target: "roki",
                    repo_id = %repo.id,
                    identifier = %repo.repo,
                    "repo health check passed"
                );
                healthy.push(repo.clone());
            }
            Err(_) => {
                info!(
                    target: "roki",
                    repo_id = %repo.id,
                    identifier = %repo.repo,
                    reason = UnhealthyReason::InvalidGhqIdentifier.name(),
                    "repo marked unhealthy; refusing to schedule work for it"
                );
                unhealthy.push(UnhealthyRepo {
                    repo_id: repo.id.clone(),
                    identifier: repo.repo.clone(),
                    reason: UnhealthyReason::InvalidGhqIdentifier,
                });
            }
        }
    }

    RepoHealth { healthy, unhealthy }
}

/// Route a Linear issue to exactly one configured repository, or `None` if
/// no scope matches.
///
/// Precedence: `LinearScope::Labels` matches outrank `LinearScope::Team`
/// matches; ties are broken by lexicographic `repo.id`. Every decision —
/// including "ignored, no scope matched" — is logged with structured fields
/// (`repo_id_chosen`, `repos_considered`, `precedence_reason`) so operators
/// can audit the routing path (Requirement 2.2).
pub fn route_issue(repos: &[RepoConfig], issue: &IssueRouteInput<'_>) -> RouteOutcome {
    // Collect every repo whose scope matches, paired with the specificity tier.
    let mut candidates: Vec<(&RepoConfig, Specificity)> = repos
        .iter()
        .filter_map(|repo| match_scope(&repo.scope, issue).map(|tier| (repo, tier)))
        .collect();

    if candidates.is_empty() {
        info!(
            target: "roki",
            issue_id = %issue.issue_id,
            team_key = %issue.team_key,
            repos_considered = repos.len(),
            precedence_reason = "no_matching_scope",
            "issue ignored: no configured repository scope matched"
        );
        return None;
    }

    // Sort: specificity tier descending, then repo.id ascending.
    candidates.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.0.id.cmp(&right.0.id))
    });

    let (winner, tier) = candidates[0];
    let precedence_reason = if candidates.len() == 1 {
        "single_match"
    } else if candidates.iter().filter(|entry| entry.1 == tier).count() == 1 {
        // Multiple candidates but only one at the top tier: specificity wins.
        tier.name()
    } else {
        // Multiple candidates tied at the top tier: id-order tie-break.
        "tie_break_repo_id_lex"
    };

    let repos_considered: Vec<&str> = candidates
        .iter()
        .map(|(repo, _)| repo.id.as_str())
        .collect();

    info!(
        target: "roki",
        issue_id = %issue.issue_id,
        team_key = %issue.team_key,
        repo_id_chosen = %winner.id,
        repos_considered = ?repos_considered,
        precedence_reason = precedence_reason,
        "issue routed to repository by deterministic precedence rule"
    );

    Some((winner.id.clone(), issue.issue_id.to_string()))
}

fn match_scope(scope: &LinearScope, issue: &IssueRouteInput<'_>) -> Option<Specificity> {
    match scope {
        LinearScope::Team { key } => {
            if key == issue.team_key {
                Some(Specificity::Team)
            } else {
                None
            }
        }
        LinearScope::Labels { any_of } => {
            let any_match = any_of
                .iter()
                .any(|label| issue.labels.iter().any(|issue_label| issue_label == label));
            if any_match {
                Some(Specificity::Labels)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn repo(id: &str, scope: LinearScope) -> RepoConfig {
        RepoConfig {
            id: id.to_string(),
            repo: format!("owner/{id}"),
            scope,
            workflow_path: PathBuf::from(format!("/srv/git/{id}/WORKFLOW.md")),
            webhook_secret_env: None,
            webhook_secret: None,
        }
    }

    #[test]
    fn overlapping_scopes_route_to_exactly_one_repo_via_specificity() {
        // Two repos match the same issue: one by team, one by label. The
        // label match is more specific and must win. Observable completion
        // criterion of task 1.5.
        let repos = vec![
            repo(
                "alpha",
                LinearScope::Team {
                    key: "ENG".to_string(),
                },
            ),
            repo(
                "beta",
                LinearScope::Labels {
                    any_of: vec!["frontend".to_string()],
                },
            ),
        ];
        let issue = IssueRouteInput {
            issue_id: "ENG-42",
            team_key: "ENG",
            labels: &["frontend".to_string()],
        };

        let outcome = route_issue(&repos, &issue);
        assert_eq!(outcome, Some(("beta".to_string(), "ENG-42".to_string())));
    }

    #[test]
    fn overlapping_scopes_at_same_tier_break_tie_by_repo_id() {
        // Both repos match by label; lexicographic `repo.id` decides.
        let repos = vec![
            repo(
                "zulu",
                LinearScope::Labels {
                    any_of: vec!["frontend".to_string()],
                },
            ),
            repo(
                "alpha",
                LinearScope::Labels {
                    any_of: vec!["frontend".to_string()],
                },
            ),
        ];
        let issue = IssueRouteInput {
            issue_id: "ENG-7",
            team_key: "ENG",
            labels: &["frontend".to_string()],
        };

        let outcome = route_issue(&repos, &issue);
        assert_eq!(outcome, Some(("alpha".to_string(), "ENG-7".to_string())));
    }

    #[test]
    fn returns_none_when_no_scope_matches() {
        let repos = vec![repo(
            "alpha",
            LinearScope::Team {
                key: "OTHER".to_string(),
            },
        )];
        let issue = IssueRouteInput {
            issue_id: "ENG-1",
            team_key: "ENG",
            labels: &[],
        };

        assert!(route_issue(&repos, &issue).is_none());
    }

    #[test]
    fn route_issue_emits_routed_event_with_required_fields() {
        // Same intent as the previous test but structured so we keep
        // ownership of the capture buffer for assertion.
        use tracing::subscriber::with_default;
        use tracing::{Event, Subscriber};
        use tracing_subscriber::field::Visit;

        #[derive(Default, Debug)]
        struct Captured {
            repo_id_chosen: Option<String>,
            repos_considered_present: bool,
            precedence_reason: Option<String>,
            saw_routed_event: bool,
        }

        struct CaptureVisitor<'a>(&'a mut Captured);

        impl Visit for CaptureVisitor<'_> {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                match field.name() {
                    "repo_id_chosen" => self.0.repo_id_chosen = Some(value.to_string()),
                    "precedence_reason" => self.0.precedence_reason = Some(value.to_string()),
                    _ => {}
                }
            }

            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}");
                match field.name() {
                    "repo_id_chosen" => self.0.repo_id_chosen = Some(rendered),
                    "repos_considered" => self.0.repos_considered_present = true,
                    "precedence_reason" => self.0.precedence_reason = Some(rendered),
                    "message" => {
                        if rendered.contains("issue routed to repository") {
                            self.0.saw_routed_event = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        struct CaptureSubscriber {
            inner: std::sync::Arc<std::sync::Mutex<Captured>>,
        }

        impl Subscriber for CaptureSubscriber {
            fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _id: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _id: &tracing::span::Id, _follows: &tracing::span::Id) {}
            fn event(&self, event: &Event<'_>) {
                let mut guard = self.inner.lock().unwrap();
                let mut visitor = CaptureVisitor(&mut guard);
                event.record(&mut visitor);
            }
            fn enter(&self, _id: &tracing::span::Id) {}
            fn exit(&self, _id: &tracing::span::Id) {}
        }

        let captured = std::sync::Arc::new(std::sync::Mutex::new(Captured::default()));
        let subscriber = CaptureSubscriber {
            inner: captured.clone(),
        };

        let repos = vec![
            repo(
                "alpha",
                LinearScope::Team {
                    key: "ENG".to_string(),
                },
            ),
            repo(
                "beta",
                LinearScope::Labels {
                    any_of: vec!["frontend".to_string()],
                },
            ),
        ];
        let issue = IssueRouteInput {
            issue_id: "ENG-42",
            team_key: "ENG",
            labels: &["frontend".to_string()],
        };

        with_default(subscriber, || {
            let outcome = route_issue(&repos, &issue);
            assert_eq!(outcome, Some(("beta".to_string(), "ENG-42".to_string())));
        });

        let snapshot = captured.lock().unwrap();
        assert!(
            snapshot.saw_routed_event,
            "expected a routed-event message; captured: {snapshot:?}"
        );
        assert_eq!(
            snapshot.repo_id_chosen.as_deref(),
            Some("beta"),
            "repo_id_chosen field missing or wrong; captured: {snapshot:?}"
        );
        assert!(
            snapshot.repos_considered_present,
            "repos_considered field missing; captured: {snapshot:?}"
        );
        assert_eq!(
            snapshot.precedence_reason.as_deref(),
            Some("labels_match"),
            "precedence_reason field missing or wrong; captured: {snapshot:?}"
        );
    }

    #[test]
    fn malformed_ghq_identifier_is_classified_unhealthy() {
        // Task 6.1: pre-existing path-based health checks moved to runtime
        // (delegated to ghq). The remaining startup check is identifier
        // shape — a single-token "repo" without `<owner>/<repo>` is
        // refused.
        let repos = vec![RepoConfig {
            id: "ghost".to_string(),
            repo: "no-owner".to_string(),
            scope: LinearScope::Team {
                key: "ENG".to_string(),
            },
            workflow_path: PathBuf::from("/dev/null"),
            webhook_secret_env: None,
            webhook_secret: None,
        }];

        let health = classify_repo_health(&repos);
        assert!(health.healthy.is_empty());
        assert_eq!(health.unhealthy.len(), 1);
        assert_eq!(
            health.unhealthy[0].reason,
            UnhealthyReason::InvalidGhqIdentifier,
        );
        assert_eq!(health.unhealthy[0].repo_id, "ghost");
    }

    #[test]
    fn well_formed_owner_repo_is_healthy() {
        let _ignored = tempdir().expect("tempdir is harmless even though we no longer probe it");
        let repos = vec![RepoConfig {
            id: "ok".to_string(),
            repo: "owner/ok".to_string(),
            scope: LinearScope::Team {
                key: "ENG".to_string(),
            },
            workflow_path: PathBuf::from("/dev/null"),
            webhook_secret_env: None,
            webhook_secret: None,
        }];

        let health = classify_repo_health(&repos);
        assert!(health.unhealthy.is_empty());
        assert_eq!(health.healthy.len(), 1);
        assert_eq!(health.healthy[0].id, "ok");
    }

    #[test]
    fn host_owner_repo_form_is_healthy() {
        let repos = vec![RepoConfig {
            id: "ok".to_string(),
            repo: "github.com/owner/ok".to_string(),
            scope: LinearScope::Team {
                key: "ENG".to_string(),
            },
            workflow_path: PathBuf::from("/dev/null"),
            webhook_secret_env: None,
            webhook_secret: None,
        }];

        let health = classify_repo_health(&repos);
        assert!(health.unhealthy.is_empty());
        assert_eq!(health.healthy.len(), 1);
    }

    #[test]
    fn classify_repo_health_keeps_serving_remaining_repos_when_one_is_unhealthy() {
        let repos = vec![
            RepoConfig {
                id: "good".to_string(),
                repo: "owner/good".to_string(),
                scope: LinearScope::Team {
                    key: "ENG".to_string(),
                },
                workflow_path: PathBuf::from("/dev/null"),
                webhook_secret_env: None,
                webhook_secret: None,
            },
            RepoConfig {
                id: "bad".to_string(),
                repo: "/abs/no-good".to_string(),
                scope: LinearScope::Team {
                    key: "OPS".to_string(),
                },
                workflow_path: PathBuf::from("/dev/null"),
                webhook_secret_env: None,
                webhook_secret: None,
            },
        ];

        let health = classify_repo_health(&repos);
        assert_eq!(health.healthy.len(), 1);
        assert_eq!(health.healthy[0].id, "good");
        assert_eq!(health.unhealthy.len(), 1);
        assert_eq!(health.unhealthy[0].repo_id, "bad");
    }
}
