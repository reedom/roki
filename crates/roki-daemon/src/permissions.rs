//! Permission strategy resolution for orchestrator and phase subprocess
//! launches.
//!
//! Two surfaces meet here:
//! 1. Phase subprocess strategy — operator-configurable via WORKFLOW.md.
//!    Default to `--settings` JSON allowlist; `--dangerously-skip-permissions`
//!    is an explicit opt-in fallback that emits a warn log per launch.
//! 2. Orchestrator strategy — daemon-pinned `--settings` over the
//!    orchestrator's `allowed_tools`, ReadOnly sandbox, elicitations rejected.
//!    The dangerous-skip fallback is intentionally not honored at the
//!    orchestrator launch site.
//!
//! Spec refs: requirements.md Req 5.12, 7.1, 9.1, 9.2, 9.3, 9.4, 9.5, 9.6.

use std::path::PathBuf;

use thiserror::Error;
use tracing::warn;

use crate::engine::phase_subprocess::catalog::PhaseName;

/// Two phase-subprocess permission shapes the daemon can hand to `claude
/// --settings <path>` or `claude --dangerously-skip-permissions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionStrategy {
    SettingsAllowlist { settings_path: PathBuf },
    DangerouslySkipPermissions,
}

/// Sandbox model passed alongside the strategy to the spawn primitive. The
/// orchestrator session is always [`Sandbox::ReadOnly`]; phase subprocesses
/// default to [`Sandbox::WorkspaceWrite`] but operators may override that
/// (per Req 9.4) — except for Classify, which is pinned to the read-only
/// trio in [`PermissionResolver::resolve_for_phase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
}

/// Final resolved permission descriptor handed to the spawn primitive.
///
/// `allowed_tools` is `None` when the strategy is
/// [`PermissionStrategy::DangerouslySkipPermissions`] and otherwise carries
/// the canonical allowlist for the launch surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPermission {
    pub strategy: PermissionStrategy,
    pub sandbox: Sandbox,
    pub reject_elicitations: bool,
    pub allowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PermissionConfigError {
    /// Phase-subprocess strategy was not configured. Startup must refuse
    /// rather than silently launch with an unconstrained sandbox (Req 9.5).
    #[error(
        "no phase-subprocess permission strategy configured: set either \
         `extension.phase.<name>.settings_path` or explicitly opt in via \
         `extension.phase.<name>.dangerously_skip_permissions = true`"
    )]
    PhaseStrategyMissing,
}

/// Operator-controlled phase-subprocess permission strategy: default is
/// `SettingsAllowlist`; the `dangerously_skip_override` flag opts in to the
/// `--dangerously-skip-permissions` fallback for non-Classify phases.
#[derive(Debug, Clone)]
pub struct PermissionResolver {
    default: Option<PermissionStrategy>,
    dangerously_skip_override: bool,
    /// Operator-set sandbox override for non-Classify phases. `None` keeps
    /// the documented default of `WorkspaceWrite`.
    sandbox_override: Option<Sandbox>,
    /// Operator-set elicitation override for non-Classify phases. `None`
    /// keeps `reject_elicitations = true`.
    reject_elicitations_override: Option<bool>,
    /// Operator-broad phase-subprocess allowlist (used for non-Classify
    /// phases). Classify pins to `{Read,Glob,Grep}` regardless.
    phase_allowed_tools: Vec<String>,
}

impl PermissionResolver {
    /// Construct a resolver with a phase-subprocess `--settings` strategy
    /// and the documented defaults.
    pub fn with_settings_path(
        settings_path: PathBuf,
        phase_allowed_tools: Vec<String>,
    ) -> Self {
        Self {
            default: Some(PermissionStrategy::SettingsAllowlist { settings_path }),
            dangerously_skip_override: false,
            sandbox_override: None,
            reject_elicitations_override: None,
            phase_allowed_tools,
        }
    }

    /// Construct a resolver with no phase-subprocess strategy configured.
    /// `resolve_for_phase` and `ensure_phase_strategy_present` fail with
    /// [`PermissionConfigError::PhaseStrategyMissing`].
    pub fn empty() -> Self {
        Self {
            default: None,
            dangerously_skip_override: false,
            sandbox_override: None,
            reject_elicitations_override: None,
            phase_allowed_tools: Vec::new(),
        }
    }

    pub fn with_dangerously_skip_override(mut self, on: bool) -> Self {
        self.dangerously_skip_override = on;
        self
    }

    pub fn with_sandbox_override(mut self, sandbox: Option<Sandbox>) -> Self {
        self.sandbox_override = sandbox;
        self
    }

    pub fn with_reject_elicitations_override(mut self, override_value: Option<bool>) -> Self {
        self.reject_elicitations_override = override_value;
        self
    }

    /// Surfaces the missing-strategy refusal at startup so the daemon does
    /// not partially come up with no sandboxing on the phase launch path.
    pub fn ensure_phase_strategy_present(&self) -> Result<(), PermissionConfigError> {
        if self.default.is_some() || self.dangerously_skip_override {
            Ok(())
        } else {
            Err(PermissionConfigError::PhaseStrategyMissing)
        }
    }

    /// Resolve the permission descriptor for a phase launch.
    ///
    /// Behavior:
    /// - `Classify`: tool surface pinned to `{Read,Glob,Grep}`,
    ///   sandbox forced to `ReadOnly`, elicitations rejected. Operator's
    ///   broader phase configuration is intentionally not honored here per
    ///   Req 5.12 / Req 7.1.
    /// - Non-Classify: honors operator overrides for sandbox /
    ///   reject_elicitations / dangerously-skip-permissions; emits a warn
    ///   log when the dangerous-skip path is selected.
    pub fn resolve_for_phase(
        &self,
        phase: PhaseName,
    ) -> Result<ResolvedPermission, PermissionConfigError> {
        if matches!(phase, PhaseName::Classify) {
            // The Classify phase deliberately ignores operator overrides:
            // the documented ReadOnly + {Read,Glob,Grep} surface is part of
            // the spec contract.
            let strategy = self
                .default
                .clone()
                .or({
                    if self.dangerously_skip_override {
                        Some(PermissionStrategy::DangerouslySkipPermissions)
                    } else {
                        None
                    }
                })
                .ok_or(PermissionConfigError::PhaseStrategyMissing)?;

            return Ok(ResolvedPermission {
                strategy,
                sandbox: Sandbox::ReadOnly,
                reject_elicitations: true,
                allowed_tools: Some(classify_allowed_tools()),
            });
        }

        let strategy = if self.dangerously_skip_override {
            warn!(
                phase = ?phase,
                "phase subprocess launching with --dangerously-skip-permissions per operator override"
            );
            PermissionStrategy::DangerouslySkipPermissions
        } else {
            self.default
                .clone()
                .ok_or(PermissionConfigError::PhaseStrategyMissing)?
        };

        let sandbox = self.sandbox_override.unwrap_or(Sandbox::WorkspaceWrite);
        let reject_elicitations = self.reject_elicitations_override.unwrap_or(true);
        let allowed_tools = match &strategy {
            PermissionStrategy::DangerouslySkipPermissions => None,
            PermissionStrategy::SettingsAllowlist { .. } => {
                Some(self.phase_allowed_tools.clone())
            }
        };
        Ok(ResolvedPermission {
            strategy,
            sandbox,
            reject_elicitations,
            allowed_tools,
        })
    }

    /// Daemon-pinned orchestrator session permission descriptor. Always
    /// `--settings` (no dangerous-skip honored), `ReadOnly`, elicitations
    /// rejected. The `--settings` payload reflects
    /// `extension.orchestrator.allowed_tools` per Req 9.6.
    pub fn resolve_for_orchestrator(orchestrator_allowed_tools: &[String]) -> ResolvedPermission {
        ResolvedPermission {
            // The settings_path here is a stable sentinel rendered at spawn
            // time by the orchestrator-session adapter — the daemon writes a
            // tempfile with the rendered allowlist and points `--settings`
            // at it. The path is intentionally not derived from operator
            // input so the dangerous-skip override cannot leak in.
            strategy: PermissionStrategy::SettingsAllowlist {
                settings_path: PathBuf::from("<orchestrator-settings>"),
            },
            sandbox: Sandbox::ReadOnly,
            reject_elicitations: true,
            allowed_tools: Some(orchestrator_allowed_tools.to_vec()),
        }
    }
}

/// Canonical Classify tool surface — pinned for the Classify phase
/// regardless of operator-broader phase-subprocess configuration.
pub fn classify_allowed_tools() -> Vec<String> {
    vec!["Read".to_owned(), "Glob".to_owned(), "Grep".to_owned()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_test::traced_test;

    fn baseline(settings: &str) -> PermissionResolver {
        PermissionResolver::with_settings_path(
            PathBuf::from(settings),
            vec!["Read".to_owned(), "Bash".to_owned(), "Edit".to_owned()],
        )
    }

    #[test]
    fn default_phase_strategy_is_workspace_write_with_rejected_elicitations() {
        let resolver = baseline("/tmp/phase.json");
        let resolved = resolver
            .resolve_for_phase(PhaseName::Implement)
            .expect("default strategy resolves");
        assert_eq!(resolved.sandbox, Sandbox::WorkspaceWrite);
        assert!(resolved.reject_elicitations);
        assert!(matches!(
            resolved.strategy,
            PermissionStrategy::SettingsAllowlist { .. }
        ));
    }

    #[test]
    fn settings_strategy_carries_configured_path() {
        let resolver = baseline("/etc/roki/phase-allow.json");
        let resolved = resolver.resolve_for_phase(PhaseName::Implement).unwrap();
        match resolved.strategy {
            PermissionStrategy::SettingsAllowlist { settings_path } => {
                assert_eq!(settings_path, PathBuf::from("/etc/roki/phase-allow.json"));
            }
            other => panic!("expected SettingsAllowlist, got {other:?}"),
        }
    }

    #[traced_test]
    #[test]
    fn dangerously_skip_override_yields_skip_strategy_and_warns() {
        let resolver = baseline("/tmp/phase.json").with_dangerously_skip_override(true);
        let resolved = resolver.resolve_for_phase(PhaseName::Implement).unwrap();
        assert_eq!(
            resolved.strategy,
            PermissionStrategy::DangerouslySkipPermissions
        );
        assert!(resolved.allowed_tools.is_none());
        assert!(logs_contain("--dangerously-skip-permissions"));
    }

    #[test]
    fn orchestrator_pin_is_readonly_settings_with_configured_allowed_tools() {
        let allowed = vec!["mcp__linear*".to_owned(), "Read".to_owned(), "Bash".to_owned()];
        let resolved = PermissionResolver::resolve_for_orchestrator(&allowed);
        assert_eq!(resolved.sandbox, Sandbox::ReadOnly);
        assert!(resolved.reject_elicitations);
        assert!(matches!(
            resolved.strategy,
            PermissionStrategy::SettingsAllowlist { .. }
        ));
        assert_eq!(resolved.allowed_tools.as_deref(), Some(allowed.as_slice()));
    }

    #[test]
    fn orchestrator_ignores_dangerously_skip_override() {
        // Even when the operator has flipped the dangerous-skip override at
        // the resolver level, orchestrator resolution is a pure function
        // that does not consult resolver state.
        let resolved = PermissionResolver::resolve_for_orchestrator(&[
            "Read".to_owned(),
        ]);
        assert!(matches!(
            resolved.strategy,
            PermissionStrategy::SettingsAllowlist { .. }
        ));
    }

    #[test]
    fn orchestrator_rejects_elicitations_regardless_of_workflow_overrides() {
        // Even an empty allowlist still produces a ReadOnly + reject =
        // true descriptor; nothing in WORKFLOW.md can flip these.
        let resolved = PermissionResolver::resolve_for_orchestrator(&[]);
        assert_eq!(resolved.sandbox, Sandbox::ReadOnly);
        assert!(resolved.reject_elicitations);
    }

    #[test]
    fn classify_pins_tool_surface_to_read_glob_grep() {
        // Operator's broader sandbox config must not leak into Classify.
        let resolver = baseline("/tmp/phase.json")
            .with_sandbox_override(Some(Sandbox::WorkspaceWrite))
            .with_reject_elicitations_override(Some(false));
        let resolved = resolver.resolve_for_phase(PhaseName::Classify).unwrap();
        assert_eq!(resolved.sandbox, Sandbox::ReadOnly);
        assert!(resolved.reject_elicitations);
        assert_eq!(
            resolved.allowed_tools.as_deref(),
            Some([
                "Read".to_owned(),
                "Glob".to_owned(),
                "Grep".to_owned(),
            ].as_slice()),
        );
    }

    #[test]
    fn missing_strategy_refuses_with_actionable_message() {
        let resolver = PermissionResolver::empty();
        let err = resolver.resolve_for_phase(PhaseName::Implement).unwrap_err();
        assert_eq!(err, PermissionConfigError::PhaseStrategyMissing);
        let rendered = err.to_string();
        assert!(rendered.contains("permission strategy"));
        assert!(rendered.contains("settings_path") || rendered.contains("settings"));
        assert!(rendered.contains("dangerously_skip_permissions"));

        // ensure_phase_strategy_present is the documented startup gate.
        assert_eq!(
            resolver.ensure_phase_strategy_present().unwrap_err(),
            PermissionConfigError::PhaseStrategyMissing,
        );
    }

    #[test]
    fn operator_sandbox_override_propagates_for_non_classify() {
        let resolver = baseline("/tmp/phase.json")
            .with_sandbox_override(Some(Sandbox::ReadOnly))
            .with_reject_elicitations_override(Some(false));
        let resolved = resolver.resolve_for_phase(PhaseName::Implement).unwrap();
        assert_eq!(resolved.sandbox, Sandbox::ReadOnly);
        assert!(!resolved.reject_elicitations);
    }
}
