//! Permission strategy resolver for worker launches.
//!
//! This module implements task 2.9 of the roki-mvp spec. It owns the logic
//! that, at every worker launch, combines the operator's selection (from
//! `Config::permission_strategy`) with any per-repo override declared in
//! `WORKFLOW.md`, and produces the effective `ResolvedPermission` that the
//! engine adapter will pass to `claude`.
//!
//! Requirements traced here:
//!
//! * 9.1 — workspace-write + reject-elicitations are the always-on defaults.
//! * 9.2 — `WORKFLOW.md` overrides apply only to workers serving that repo.
//! * 9.3 — `--settings` allowlist strategy is forwarded to the worker.
//! * 9.4 — `--dangerously-skip-permissions` fallback is forwarded AND logged
//!   on every worker launch.
//! * 9.5 — refuse to launch a worker when neither strategy is configured.
//!
//! Design note (design.md "Permissions"): this resolver is a pure decision
//! function plus a single `tracing::warn!` event for the dangerous fallback.
//! It does not perform any I/O, does not touch the filesystem, and does not
//! launch subprocesses; those are the engine adapter's concerns.

use std::path::PathBuf;

use crate::config::PermissionStrategy;
use crate::workflow::{ElicitationsMode, SandboxMode};

/// Default sandbox mode applied to every worker (Requirement 9.1).
pub const DEFAULT_SANDBOX: SandboxMode = SandboxMode::WorkspaceWrite;

/// Default elicitation policy applied to every worker (Requirement 9.1).
pub const DEFAULT_ELICITATIONS: ElicitationsMode = ElicitationsMode::Reject;

/// The "mode" half of an effective permission strategy: how the worker is
/// authorised to take privileged actions.
///
/// This is intentionally a thin re-shape of [`PermissionStrategy`] from
/// `config`; the resolver may receive a `mode` either from the operator's
/// global selection or from a per-repo `WORKFLOW.md` override, so both
/// sources need to flow through the same enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionMode {
    /// `--settings` allowlist; the path points at a Claude Code settings file.
    /// Requirement 9.3.
    Allowlist { settings_path: PathBuf },
    /// `--dangerously-skip-permissions` fallback. Requirement 9.4.
    DangerousFallback,
}

impl From<PermissionStrategy> for PermissionMode {
    fn from(strategy: PermissionStrategy) -> Self {
        match strategy {
            PermissionStrategy::Allowlist { settings_path } => Self::Allowlist { settings_path },
            PermissionStrategy::DangerouslySkipPermissions => Self::DangerousFallback,
        }
    }
}

/// Source that produced the resolved [`PermissionMode`]. Recorded for the
/// dangerous-fallback warn log (Requirement 9.4) and useful for future
/// observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionSource {
    /// The operator's global selection (`Config::permission_strategy`).
    Operator,
    /// A per-repo `WORKFLOW.md` override.
    WorkflowOverride,
}

impl PermissionSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::WorkflowOverride => "workflow_override",
        }
    }
}

/// Per-repo permission override, sourced from `WORKFLOW.md`.
///
/// Every field is optional — an absent field means "fall through to the
/// operator-level default". Documenting this here avoids implicit semantics
/// at the call site.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoPermissionOverride {
    /// Override the permission mode (Requirement 9.2 in conjunction with 9.3
    /// or 9.4).
    pub mode: Option<PermissionMode>,
    /// Override the sandbox default. Absent leaves the operator default in
    /// place.
    pub sandbox: Option<SandboxMode>,
    /// Override the elicitation policy. Absent leaves the default in place.
    pub elicitations: Option<ElicitationsMode>,
}

impl RepoPermissionOverride {
    /// Sentinel for "no per-repo override declared".
    pub const NONE: Self = Self {
        mode: None,
        sandbox: None,
        elicitations: None,
    };
}

/// The fully resolved permission strategy passed to the engine adapter at
/// worker launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPermission {
    /// How the worker authorises privileged actions (Requirement 9.3, 9.4).
    pub mode: PermissionMode,
    /// Sandbox mode in effect for this worker (Requirement 9.1, 9.2).
    pub sandbox: SandboxMode,
    /// Elicitation policy in effect for this worker (Requirement 9.1, 9.2).
    pub elicitations: ElicitationsMode,
    /// Where `mode` came from. Used by [`PermissionResolver::resolve`] to
    /// build the dangerous-fallback warn log (Requirement 9.4).
    pub mode_source: PermissionSource,
}

/// Errors raised by [`PermissionResolver::resolve`].
#[derive(Debug, thiserror::Error)]
pub enum PermissionConfigError {
    /// Neither the operator nor a per-repo override declared a permission
    /// mode. Requirement 9.5.
    #[error(
        "no permission strategy configured for repo `{repo}`: set the operator-level \
         strategy in config or declare an override in WORKFLOW.md"
    )]
    NeitherStrategyConfigured { repo: String },
}

/// Resolver that combines the operator-level selection with a per-repo
/// override into a [`ResolvedPermission`] at worker-launch time.
///
/// Construction takes the operator-level inputs once; the `resolve` call is
/// invoked per launch with the relevant repo's override (which may be
/// [`RepoPermissionOverride::NONE`]).
#[derive(Debug, Clone)]
pub struct PermissionResolver {
    /// Operator-level mode from `Config::permission_strategy`. Absent only in
    /// tests that exercise the "no-strategy" path; production code receives
    /// `Some` by virtue of `Config` validation (Requirement 9.5 at the
    /// config layer).
    operator_mode: Option<PermissionMode>,
    default_sandbox: SandboxMode,
    default_elicitations: ElicitationsMode,
}

impl PermissionResolver {
    /// Build a resolver from the operator-level configuration.
    ///
    /// `operator_mode` corresponds to `Config::permission_strategy`. Pass
    /// `None` only to model the not-yet-configured case in tests; production
    /// callers always have a `Some` here because `Config` validation refuses
    /// to load without a permission strategy.
    pub fn new(operator_mode: Option<PermissionMode>) -> Self {
        Self {
            operator_mode,
            default_sandbox: DEFAULT_SANDBOX,
            default_elicitations: DEFAULT_ELICITATIONS,
        }
    }

    /// Resolve the effective permission strategy for a single worker launch.
    ///
    /// Precedence rules (documented and exercised by the matrix tests):
    ///
    /// * `repo_override.mode` (if `Some`) wins over `operator_mode`. This
    ///   honours Requirement 9.2: a per-repo override applies "only for
    ///   workers serving the corresponding repository".
    /// * Sandbox and elicitations come from the override when present, else
    ///   from the resolver-level defaults (`workspace-write`, `reject`).
    ///   Requirement 9.1.
    /// * If neither operator nor override declared a `mode`, return
    ///   [`PermissionConfigError::NeitherStrategyConfigured`]. Requirement 9.5.
    /// * When the resolved `mode` is `DangerousFallback`, emit a
    ///   `tracing::warn!` event tagged with the repo and the source of the
    ///   decision, exactly once per launch. Requirement 9.4.
    pub fn resolve(
        &self,
        repo: &str,
        repo_override: &RepoPermissionOverride,
    ) -> Result<ResolvedPermission, PermissionConfigError> {
        let (mode, mode_source) = match repo_override.mode.clone() {
            Some(mode) => (mode, PermissionSource::WorkflowOverride),
            None => match self.operator_mode.clone() {
                Some(mode) => (mode, PermissionSource::Operator),
                None => {
                    return Err(PermissionConfigError::NeitherStrategyConfigured {
                        repo: repo.to_string(),
                    });
                }
            },
        };

        let sandbox = repo_override.sandbox.unwrap_or(self.default_sandbox);
        let elicitations = repo_override
            .elicitations
            .unwrap_or(self.default_elicitations);

        if matches!(mode, PermissionMode::DangerousFallback) {
            // Requirement 9.4: log the elevated-permission decision per
            // worker launch. The structured fields let the operator audit
            // every dangerous launch and identify whether the override or
            // the operator selection drove the decision.
            tracing::warn!(
                repo = %repo,
                mode = "dangerous_fallback",
                source = mode_source.as_str(),
                "launching worker with dangerously-skip-permissions"
            );
        }

        Ok(ResolvedPermission {
            mode,
            sandbox,
            elicitations,
            mode_source,
        })
    }
}

#[cfg(test)]
mod tests {
    //! Observable-completion test set for task 2.9.
    //!
    //! The four operator-level matrix cells (allowlist on or off; dangerous
    //! on or off; per-repo override absent) are covered first. The override
    //! variants follow.

    use super::*;
    use std::path::PathBuf;
    use tracing_test::traced_test;

    fn allowlist_path() -> PathBuf {
        PathBuf::from("/etc/roki/claude-settings.json")
    }

    fn allowlist_mode() -> PermissionMode {
        PermissionMode::Allowlist {
            settings_path: allowlist_path(),
        }
    }

    // ---------- 4-cell operator matrix (override absent) ----------

    #[test]
    fn cell1_allowlist_on_dangerous_off_no_override_resolves_allowlist() {
        // Cell 1: allowlist=on, dangerous=off, override=absent.
        let resolver = PermissionResolver::new(Some(allowlist_mode()));

        let resolved = resolver
            .resolve("repo-a", &RepoPermissionOverride::NONE)
            .expect("allowlist alone must resolve");

        assert_eq!(resolved.mode, allowlist_mode());
        assert_eq!(resolved.mode_source, PermissionSource::Operator);
        assert_eq!(resolved.sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(resolved.elicitations, ElicitationsMode::Reject);
    }

    #[test]
    #[traced_test]
    fn cell2_allowlist_off_dangerous_on_no_override_resolves_dangerous_with_warn_log() {
        // Cell 2: allowlist=off, dangerous=on, override=absent.
        // Requirement 9.4: dangerous fallback must emit a structured warn log
        // identifying the repo and the source of the decision.
        let resolver = PermissionResolver::new(Some(PermissionMode::DangerousFallback));

        let resolved = resolver
            .resolve("repo-b", &RepoPermissionOverride::NONE)
            .expect("dangerous alone must resolve");

        assert_eq!(resolved.mode, PermissionMode::DangerousFallback);
        assert_eq!(resolved.mode_source, PermissionSource::Operator);
        assert_eq!(resolved.sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(resolved.elicitations, ElicitationsMode::Reject);

        // Per-launch log shape: WARN level, mentions the repo, the
        // structured `mode` tag, and the source. `tracing-test` captures
        // every formatted log line, including structured fields.
        assert!(
            logs_contain("launching worker with dangerously-skip-permissions"),
            "expected dangerous-fallback warn log to be emitted"
        );
        assert!(
            logs_contain("repo=repo-b"),
            "warn log must include the repo as a structured field"
        );
        assert!(
            logs_contain("mode=\"dangerous_fallback\""),
            "warn log must tag the mode as `dangerous_fallback`"
        );
        assert!(
            logs_contain("source=\"operator\""),
            "warn log must record the operator as the decision source"
        );
    }

    #[test]
    fn cell3_allowlist_on_dangerous_on_no_override_picks_documented_winner() {
        // Cell 3: allowlist=on, dangerous=on, override=absent.
        //
        // The operator-level `Config` only ever holds ONE
        // `PermissionStrategy` (see `config::PermissionStrategy`), so this
        // matrix cell can only be observed via downstream re-shaping. The
        // documented winner inside this resolver is "allowlist takes
        // precedence over dangerous": if the operator passes an allowlist,
        // the resolver uses it. The dangerous-only operator path is covered
        // by Cell 2.
        let resolver = PermissionResolver::new(Some(allowlist_mode()));

        let resolved = resolver
            .resolve("repo-c", &RepoPermissionOverride::NONE)
            .expect("allowlist must resolve when both are conceptually `on`");

        assert_eq!(
            resolved.mode,
            allowlist_mode(),
            "allowlist must take precedence over dangerous at the operator level"
        );
        assert_eq!(resolved.mode_source, PermissionSource::Operator);
    }

    #[test]
    fn cell4_allowlist_off_dangerous_off_no_override_refuses_to_launch() {
        // Cell 4: allowlist=off, dangerous=off, override=absent.
        // Requirement 9.5: refuse to launch when neither strategy is
        // configured.
        let resolver = PermissionResolver::new(None);

        let err = resolver
            .resolve("repo-d", &RepoPermissionOverride::NONE)
            .expect_err("missing both strategies must refuse to launch");

        match err {
            PermissionConfigError::NeitherStrategyConfigured { repo } => {
                assert_eq!(repo, "repo-d");
            }
        }
    }

    // ---------- override variants ----------

    #[test]
    fn override_allowlist_supersedes_operator_dangerous() {
        // Override sets allowlist with operator dangerous → override wins,
        // allowlist resolved.
        let resolver = PermissionResolver::new(Some(PermissionMode::DangerousFallback));

        let repo_override = RepoPermissionOverride {
            mode: Some(allowlist_mode()),
            ..RepoPermissionOverride::NONE
        };

        let resolved = resolver
            .resolve("repo-e", &repo_override)
            .expect("override allowlist must resolve");

        assert_eq!(resolved.mode, allowlist_mode());
        assert_eq!(resolved.mode_source, PermissionSource::WorkflowOverride);
    }

    #[test]
    #[traced_test]
    fn override_dangerous_supersedes_operator_allowlist_with_warn_log() {
        // Override sets dangerous with operator allowlist → override wins,
        // dangerous resolved + warn log tagged with the override source.
        let resolver = PermissionResolver::new(Some(allowlist_mode()));

        let repo_override = RepoPermissionOverride {
            mode: Some(PermissionMode::DangerousFallback),
            ..RepoPermissionOverride::NONE
        };

        let resolved = resolver
            .resolve("repo-f", &repo_override)
            .expect("override dangerous must resolve");

        assert_eq!(resolved.mode, PermissionMode::DangerousFallback);
        assert_eq!(resolved.mode_source, PermissionSource::WorkflowOverride);

        assert!(
            logs_contain("launching worker with dangerously-skip-permissions"),
            "expected override-driven dangerous-fallback warn log"
        );
        assert!(
            logs_contain("repo=repo-f"),
            "warn log must include the repo as a structured field"
        );
        assert!(
            logs_contain("source=\"workflow_override\""),
            "warn log must record the workflow override as the decision source"
        );
    }

    #[test]
    fn override_sandbox_and_elicitations_replace_defaults() {
        // Requirement 9.2: a per-repo `WORKFLOW.md` override applies only to
        // workers serving that repo. Sandbox and elicitations carried by the
        // override must replace the resolver defaults.
        let resolver = PermissionResolver::new(Some(allowlist_mode()));

        let repo_override = RepoPermissionOverride {
            mode: None,
            sandbox: Some(SandboxMode::ReadOnly),
            elicitations: Some(ElicitationsMode::Allow),
        };

        let resolved = resolver
            .resolve("repo-g", &repo_override)
            .expect("operator allowlist plus override defaults must resolve");

        assert_eq!(resolved.mode, allowlist_mode());
        assert_eq!(resolved.mode_source, PermissionSource::Operator);
        assert_eq!(resolved.sandbox, SandboxMode::ReadOnly);
        assert_eq!(resolved.elicitations, ElicitationsMode::Allow);
    }

    #[test]
    fn from_config_permission_strategy_round_trips_to_permission_mode() {
        // Sanity check on the adapter from the existing config type into the
        // resolver-local enum. Keeps task 1.2's `PermissionStrategy` usable
        // without a duplicate enum at the call site.
        let allowlist: PermissionMode = PermissionStrategy::Allowlist {
            settings_path: allowlist_path(),
        }
        .into();
        assert_eq!(allowlist, allowlist_mode());

        let dangerous: PermissionMode = PermissionStrategy::DangerouslySkipPermissions.into();
        assert_eq!(dangerous, PermissionMode::DangerousFallback);
    }
}
