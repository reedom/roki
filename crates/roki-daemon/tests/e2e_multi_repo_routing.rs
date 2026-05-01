//! End-to-end multi-repo routing test (task 4.4).
//!
//! This test exercises the deterministic multi-repo routing precedence rule
//! delivered by task 1.5 against the same `RepoConfig` value the production
//! configuration loader emits. It elevates the routing-decision boundary
//! into an integration-level assertion so the e2e log surface (a single
//! `routed` event with the precedence decision named) is observable from
//! the test boundary the operator would observe in production.
//!
//! Boundary notes
//!
//! * The MVP routing function (`roki_daemon::routing::route_issue`) is the
//!   only place where multi-repo arbitration happens — by design, each
//!   `LinearTracker` only sees its own scope, so the production tracker
//!   bridge never has to fan an issue across repos. The natural e2e
//!   boundary for "two repositories with overlapping Linear scopes" is
//!   therefore the routing function called against a real `RepoConfig`
//!   list.
//! * No `claude` subprocess, no `Orchestrator`, no `LinearTracker` are
//!   started here: the test stops at the routing decision and never
//!   advances any actor toward `Active`. That is the correct boundary for
//!   Requirements 2.2 and 2.4: the deterministic precedence rule, the
//!   single `(repo, issue)` key it produces, and the structured log event
//!   that names the decision.
//!
//! Determinism notes
//!
//! * Every call to `route_issue` is synchronous and pure in its inputs;
//!   there are no clocks, channels, sleeps, or background tasks in this
//!   test. Three sequential runs of the test binary therefore observe
//!   bit-identical outputs.
//!
//! Requirements: 2.2, 2.4.

use std::path::PathBuf;

use roki_daemon::config::repos::{LinearScope, RepoConfig};
use roki_daemon::routing::{IssueRouteInput, route_issue};

/// Construct a minimal `RepoConfig` for routing assertions. The production
/// loader populates more fields than `route_issue` actually inspects; the
/// router only reads `id` and `scope`, so the test pins concrete but
/// otherwise unused values for `repo` (the ghq identifier) and
/// `workflow_path`.
fn repo_config(id: &str, scope: LinearScope) -> RepoConfig {
    RepoConfig {
        id: id.to_string(),
        repo: format!("owner/{id}"),
        scope,
        workflow_path: PathBuf::from(format!("/srv/git/{id}/WORKFLOW.md")),
        webhook_secret_env: None,
        webhook_secret: None,
    }
}

/// Count the number of captured log lines that contain `needle`. Used to
/// prove "exactly one `routed` event per logical issue".
fn count_lines_with(lines: &[&str], needle: &str) -> usize {
    lines.iter().filter(|line| line.contains(needle)).count()
}

/// Routing decision when a label-scoped repo and a team-scoped repo both
/// match the same logical issue.
///
/// Precedence rule (Requirement 2.2): `LinearScope::Labels` outranks
/// `LinearScope::Team` because labels narrow the team's issue stream. The
/// label-matched repo (`beta`) wins; the team-matched repo (`alpha`)
/// observably ignores the issue (it does not appear as the chosen repo).
/// Exactly one `routed` event is emitted; it names the precedence decision
/// (`labels_match`) and lists both repositories under `repos_considered`.
#[test]
#[tracing_test::traced_test]
fn routes_overlapping_team_and_label_scopes_to_label_repo_with_single_routed_event() {
    let repos = vec![
        repo_config(
            "alpha",
            LinearScope::Team {
                key: "ENG".to_string(),
            },
        ),
        repo_config(
            "beta",
            LinearScope::Labels {
                any_of: vec!["frontend".to_string()],
            },
        ),
    ];
    let labels = vec!["frontend".to_string()];
    let issue = IssueRouteInput {
        issue_id: "ENG-42",
        team_key: "ENG",
        labels: &labels,
    };

    let outcome = route_issue(&repos, &issue);

    // Acceptance criterion 1: deterministic precedence selects exactly
    // ONE repository — the label-scoped one.
    assert_eq!(
        outcome,
        Some(("beta".to_string(), "ENG-42".to_string())),
        "labels match must outrank team match (Requirement 2.2)",
    );

    // Acceptance criterion 2: the OTHER repo is observably ignored. The
    // routing outcome key (Requirement 2.4) names exactly one repo, and
    // it is not the loser.
    let (chosen_repo, chosen_issue) = outcome.expect("outcome present");
    assert_ne!(
        chosen_repo, "alpha",
        "the team-scoped repo must be ignored when a label-scoped repo also matches",
    );
    assert_eq!(
        chosen_issue, "ENG-42",
        "the routing key must carry the issue id verbatim",
    );

    // Acceptance criterion 3: the precedence decision is logged. The
    // production `route_issue` emits a single info-level event whose
    // message contains the literal phrase below and whose structured
    // fields name the chosen repo and the precedence reason.
    assert!(
        logs_contain("issue routed to repository by deterministic precedence rule"),
        "expected the routed-event message to be emitted",
    );
    assert!(
        logs_contain("repo_id_chosen=\"beta\"") || logs_contain("repo_id_chosen=beta"),
        "log event must name the winning repo",
    );
    assert!(
        logs_contain("precedence_reason=\"labels_match\"")
            || logs_contain("precedence_reason=labels_match"),
        "log event must name the precedence reason that fired",
    );
    // `repos_considered` is logged with the full candidate list so the
    // operator can audit the loser. We only require both ids appear in
    // its rendering — order is not part of the contract.
    assert!(
        logs_contain("alpha"),
        "log event must mention the considered loser (alpha)",
    );
    assert!(
        logs_contain("repos_considered"),
        "log event must include the repos_considered field",
    );

    // Acceptance criterion 4: exactly ONE `routed` event per logical
    // issue. Re-routing the same issue would emit a second event, so we
    // assert the count is exactly one.
    logs_assert(|lines: &[&str]| {
        let count = count_lines_with(
            lines,
            "issue routed to repository by deterministic precedence rule",
        );
        if count == 1 {
            Ok(())
        } else {
            Err(format!(
                "expected exactly one routed event for the logical issue, observed {count}; \
                 captured lines: {lines:?}",
            ))
        }
    });
}

/// Routing decision when two repos match at the SAME specificity tier.
///
/// Both `mango` and `apple` match the issue's `frontend` label. The
/// precedence rule must pick a single deterministic winner — lexicographic
/// `repo.id`. `apple` wins; `mango` is observably ignored. Exactly one
/// `routed` event is emitted, naming the tie-break reason
/// (`tie_break_repo_id_lex`).
#[test]
#[tracing_test::traced_test]
fn routes_overlapping_same_tier_scopes_via_lex_repo_id_tie_break() {
    let repos = vec![
        repo_config(
            "mango",
            LinearScope::Labels {
                any_of: vec!["frontend".to_string()],
            },
        ),
        repo_config(
            "apple",
            LinearScope::Labels {
                any_of: vec!["frontend".to_string()],
            },
        ),
    ];
    let labels = vec!["frontend".to_string()];
    let issue = IssueRouteInput {
        issue_id: "ENG-7",
        team_key: "ENG",
        labels: &labels,
    };

    let outcome = route_issue(&repos, &issue);

    assert_eq!(
        outcome,
        Some(("apple".to_string(), "ENG-7".to_string())),
        "lexicographic repo.id must break ties at equal specificity (Requirement 2.2)",
    );
    let (chosen_repo, _) = outcome.expect("outcome present");
    assert_ne!(
        chosen_repo, "mango",
        "the lexicographically-later repo must be observably ignored",
    );

    // The structured event names the tie-break reason and the winner.
    assert!(
        logs_contain("issue routed to repository by deterministic precedence rule"),
        "expected the routed-event message to be emitted",
    );
    assert!(
        logs_contain("repo_id_chosen=\"apple\"") || logs_contain("repo_id_chosen=apple"),
        "log event must name the winning repo",
    );
    assert!(
        logs_contain("precedence_reason=\"tie_break_repo_id_lex\"")
            || logs_contain("precedence_reason=tie_break_repo_id_lex"),
        "log event must name the tie-break precedence reason",
    );
    assert!(
        logs_contain("mango"),
        "log event must mention the considered loser (mango)",
    );

    logs_assert(|lines: &[&str]| {
        let count = count_lines_with(
            lines,
            "issue routed to repository by deterministic precedence rule",
        );
        if count == 1 {
            Ok(())
        } else {
            Err(format!(
                "expected exactly one routed event for the logical issue, observed {count}; \
                 captured lines: {lines:?}",
            ))
        }
    });
}
