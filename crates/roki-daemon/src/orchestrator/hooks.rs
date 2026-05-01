//! Pre-cleanup hook extension surface.
//!
//! This module publishes the [`PreCleanupHook`] trait and the
//! [`HookRegistry`] that holds registered hooks and aggregates their
//! decisions. Together they are the additive extension surface pinned by
//! requirement 13.2: roki-distill-postmerge (and any future deferred-cleanup
//! consumer) registers an implementor here, the orchestrator dispatches the
//! hook on the vetoable `TerminalSuccess -> Cleaning` transition, and a
//! `Deny` decision blocks workspace removal.
//!
//! ## What ships in 2.1a
//!
//! * The [`PreCleanupHook`] trait — async, `Send + Sync + 'static`, returning
//!   a [`VetoDecision`].
//! * [`PreCleanupContext`] — the read-only payload handed to each hook.
//! * [`HookRegistry`] — a thread-safe registry that exposes
//!   [`HookRegistry::register_pre_cleanup_hook`] and
//!   [`HookRegistry::evaluate_pre_cleanup`]. The orchestrator core (task 3.x)
//!   will own a single `HookRegistry`, register hooks against it during
//!   wiring, and call `evaluate_pre_cleanup` immediately before the workspace
//!   removal step. This module deliberately does NOT touch any workspace
//!   path; the boundary for that lives in tasks 1.5 / 2.2.
//!
//! ## Aggregation policy
//!
//! When multiple hooks are registered, the registry aggregates their
//! decisions with a strict `any-Deny-blocks` rule:
//!
//! * If every hook returns [`VetoDecision::Allow`], the aggregated decision
//!   is [`VetoDecision::Allow`].
//! * If any hook returns [`VetoDecision::Deny`], the aggregated decision is
//!   the FIRST `Deny` encountered (preserving its `reason` for logging), and
//!   subsequent hooks are still invoked so each gets a chance to perform any
//!   side-effect-free observation it wants.
//!
//! Hooks are awaited sequentially in registration order. This is intentional:
//! distill-postmerge's hook may be I/O-heavy and the orchestrator must keep
//! per-`(repo, issue)` ordering deterministic for replay-friendly logs.
//!
//! ## What this module does NOT do
//!
//! * It does not call into `WorkspaceManager`. Workspace removal is a step
//!   the orchestrator core performs AFTER `evaluate_pre_cleanup` returns
//!   `Allow`; bundling that here would couple the hook surface to a
//!   downstream module.
//! * It does not publish a `SubscriptionHandle` type. The MVP registry is
//!   append-only; deregistration is not in 2.1a's scope and is deferred to
//!   the orchestrator core task that owns lifecycle.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::state::{CorrelationId, IssueId, VetoDecision};

/// Read-only payload handed to a [`PreCleanupHook`] when the orchestrator
/// dispatches the hook on the `TerminalSuccess -> Cleaning` transition.
///
/// The fields are intentionally minimal — only what every hook needs to
/// identify the issue and correlate logs. Path / workspace fields belong to
/// task 7.1d's surface (`WorktreeRegistry`) and will be added there without
/// breaking this struct (it is `non_exhaustive` so additive evolution is
/// safe). Per task 7.1b the state-machine key collapsed from `(repo, issue)`
/// to `(issue,)`; this context drops the `repo` field accordingly.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PreCleanupContext {
    pub issue: IssueId,
    pub correlation_id: CorrelationId,
}

impl PreCleanupContext {
    pub fn new(issue: IssueId, correlation_id: CorrelationId) -> Self {
        Self {
            issue,
            correlation_id,
        }
    }
}

/// Vetoable observer of the `TerminalSuccess -> Cleaning` transition.
///
/// Implementors are registered via [`HookRegistry::register_pre_cleanup_hook`]
/// and dispatched by the orchestrator core (task 3.x) before any workspace
/// removal step. Returning [`VetoDecision::Deny`] blocks workspace removal
/// and the reason is logged.
///
/// Hooks must be `Send + Sync + 'static` so they can be held in an [`Arc`]
/// trait object across the orchestrator's tokio task boundary. Hooks should
/// be cheap to clone (they typically wrap an `Arc<Inner>`).
#[async_trait]
pub trait PreCleanupHook: Send + Sync + 'static {
    /// Inspect the pre-cleanup context and return [`VetoDecision::Allow`] to
    /// permit workspace removal or [`VetoDecision::Deny`] to block it.
    ///
    /// Implementors must not mutate orchestrator state or invoke any tracker
    /// or engine API from inside this method; the contract is observation +
    /// decision only.
    async fn pre_cleanup(&self, ctx: &PreCleanupContext) -> VetoDecision;
}

/// Thread-safe registry of pre-cleanup hooks.
///
/// One instance per orchestrator. The registry owns its hooks behind an
/// `Arc<Mutex<Vec<Arc<dyn PreCleanupHook>>>>` so registration may happen
/// during daemon wiring (single-threaded) and dispatch may happen later from
/// the orchestrator's tokio task (multi-threaded).
///
/// The mutex is held only for the duration of registration or for the
/// duration of cloning the hook list at evaluation entry — the actual
/// `pre_cleanup` calls run outside the lock so a slow hook cannot block
/// concurrent registrations on a separate `(repo, issue)`.
#[derive(Default, Clone)]
pub struct HookRegistry {
    hooks: Arc<Mutex<Vec<Arc<dyn PreCleanupHook>>>>,
}

impl HookRegistry {
    /// Construct an empty registry. Equivalent to [`HookRegistry::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pre-cleanup hook. The hook is dispatched on every
    /// subsequent `TerminalSuccess -> Cleaning` transition.
    ///
    /// Returns the count of hooks registered after this call so callers can
    /// log a deterministic "registered hook #N" line if useful. The MVP
    /// registry is append-only; deregistration is not exposed in this task.
    pub fn register_pre_cleanup_hook(&self, hook: Arc<dyn PreCleanupHook>) -> usize {
        let mut guard = self
            .hooks
            .lock()
            .expect("HookRegistry mutex poisoned; this is unrecoverable");
        guard.push(hook);
        guard.len()
    }

    /// Snapshot the current hook list. Used by the orchestrator core to
    /// dispatch hooks without holding the registration mutex across `await`
    /// points.
    fn snapshot_hooks(&self) -> Vec<Arc<dyn PreCleanupHook>> {
        self.hooks
            .lock()
            .expect("HookRegistry mutex poisoned; this is unrecoverable")
            .clone()
    }

    /// Number of registered hooks. Primarily for tests and structured-log
    /// "registry depth" lines.
    pub fn len(&self) -> usize {
        self.hooks
            .lock()
            .expect("HookRegistry mutex poisoned; this is unrecoverable")
            .len()
    }

    /// Returns `true` iff no hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Dispatch every registered hook in registration order and return the
    /// aggregated [`VetoDecision`].
    ///
    /// Aggregation is strict `any-Deny-blocks`:
    ///
    /// * Empty registry returns [`VetoDecision::Allow`] (no hooks means no
    ///   downstream cleanup work to defer).
    /// * If any hook returns [`VetoDecision::Deny`], the FIRST `Deny` is the
    ///   aggregated decision; remaining hooks are still invoked so each can
    ///   perform side-effect-free observation, but their decisions cannot
    ///   override the recorded `Deny`.
    pub async fn evaluate_pre_cleanup(&self, ctx: &PreCleanupContext) -> VetoDecision {
        let hooks = self.snapshot_hooks();
        let mut aggregated = VetoDecision::Allow;
        for hook in hooks {
            let decision = hook.pre_cleanup(ctx).await;
            if aggregated.is_allow() {
                // Promote the first Deny but keep iterating so every hook
                // observes the transition.
                aggregated = decision;
            }
        }
        aggregated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    /// Deny-returning hook that records invocation count, used to assert no
    /// workspace-removal step ran. The test harness here cannot literally
    /// invoke a workspace remove (workspace lives in 2.2/4.x); instead we
    /// assert the registry's aggregated decision is `Deny` and verify the
    /// orchestrator-side "if Allow then remove" guard is well-defined by
    /// pairing the assertion with a sentinel counter.
    struct DenyingHook {
        reason: &'static str,
        invocations: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PreCleanupHook for DenyingHook {
        async fn pre_cleanup(&self, _ctx: &PreCleanupContext) -> VetoDecision {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            VetoDecision::deny(self.reason)
        }
    }

    struct AllowingHook {
        invocations: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PreCleanupHook for AllowingHook {
        async fn pre_cleanup(&self, _ctx: &PreCleanupContext) -> VetoDecision {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            VetoDecision::Allow
        }
    }

    fn sample_context() -> PreCleanupContext {
        PreCleanupContext::new(IssueId::new("ENG-1"), CorrelationId::from_uuid(Uuid::nil()))
    }

    /// Models the orchestrator-side guard so the test can assert a `Deny`
    /// decision keeps the workspace-removal step from ever running. The real
    /// orchestrator core (task 3.x) will inline this guard immediately before
    /// `WorkspaceManager::remove`; here we simulate it with a counter.
    async fn run_pre_cleanup_then_maybe_remove_workspace(
        registry: &HookRegistry,
        ctx: &PreCleanupContext,
        workspace_remove_calls: &AtomicUsize,
    ) -> VetoDecision {
        let decision = registry.evaluate_pre_cleanup(ctx).await;
        if decision.is_allow() {
            workspace_remove_calls.fetch_add(1, Ordering::SeqCst);
        }
        decision
    }

    #[tokio::test]
    async fn pre_cleanup_hook_deny_prevents_workspace_removal() {
        let registry = HookRegistry::new();
        let invocations = Arc::new(AtomicUsize::new(0));
        registry.register_pre_cleanup_hook(Arc::new(DenyingHook {
            reason: "distill-postmerge: write still pending",
            invocations: invocations.clone(),
        }));

        let workspace_remove_calls = AtomicUsize::new(0);
        let decision = run_pre_cleanup_then_maybe_remove_workspace(
            &registry,
            &sample_context(),
            &workspace_remove_calls,
        )
        .await;

        match decision {
            VetoDecision::Deny { reason } => {
                assert_eq!(reason, "distill-postmerge: write still pending");
            }
            VetoDecision::Allow => panic!("expected Deny but got Allow"),
        }
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            1,
            "the registered hook must be invoked exactly once",
        );
        assert_eq!(
            workspace_remove_calls.load(Ordering::SeqCst),
            0,
            "workspace removal must not run when the aggregated decision is Deny",
        );
    }

    #[tokio::test]
    async fn pre_cleanup_hook_allow_passes_through() {
        let registry = HookRegistry::new();
        let invocations = Arc::new(AtomicUsize::new(0));
        registry.register_pre_cleanup_hook(Arc::new(AllowingHook {
            invocations: invocations.clone(),
        }));

        let workspace_remove_calls = AtomicUsize::new(0);
        let decision = run_pre_cleanup_then_maybe_remove_workspace(
            &registry,
            &sample_context(),
            &workspace_remove_calls,
        )
        .await;

        assert!(
            decision.is_allow(),
            "single Allow hook must aggregate to Allow",
        );
        assert_eq!(invocations.load(Ordering::SeqCst), 1);
        assert_eq!(
            workspace_remove_calls.load(Ordering::SeqCst),
            1,
            "workspace removal must run when the aggregated decision is Allow",
        );
    }

    #[tokio::test]
    async fn empty_registry_aggregates_to_allow() {
        let registry = HookRegistry::new();
        assert!(registry.is_empty());
        let decision = registry.evaluate_pre_cleanup(&sample_context()).await;
        assert!(
            decision.is_allow(),
            "an empty registry must allow workspace removal by default",
        );
    }

    #[tokio::test]
    async fn multiple_hooks_aggregate_to_deny_on_any_deny() {
        let registry = HookRegistry::new();
        let allow_invocations = Arc::new(AtomicUsize::new(0));
        let deny_invocations = Arc::new(AtomicUsize::new(0));

        registry.register_pre_cleanup_hook(Arc::new(AllowingHook {
            invocations: allow_invocations.clone(),
        }));
        registry.register_pre_cleanup_hook(Arc::new(DenyingHook {
            reason: "second-hook says no",
            invocations: deny_invocations.clone(),
        }));

        let decision = registry.evaluate_pre_cleanup(&sample_context()).await;
        match decision {
            VetoDecision::Deny { reason } => assert_eq!(reason, "second-hook says no"),
            VetoDecision::Allow => panic!("any Deny must dominate"),
        }

        // Both hooks invoked: aggregation never short-circuits, so each hook
        // gets to observe the transition exactly once.
        assert_eq!(allow_invocations.load(Ordering::SeqCst), 1);
        assert_eq!(deny_invocations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn first_deny_wins_when_multiple_hooks_deny() {
        let registry = HookRegistry::new();
        let first_deny = Arc::new(AtomicUsize::new(0));
        let second_deny = Arc::new(AtomicUsize::new(0));

        registry.register_pre_cleanup_hook(Arc::new(DenyingHook {
            reason: "first-deny",
            invocations: first_deny.clone(),
        }));
        registry.register_pre_cleanup_hook(Arc::new(DenyingHook {
            reason: "second-deny",
            invocations: second_deny.clone(),
        }));

        let decision = registry.evaluate_pre_cleanup(&sample_context()).await;
        match decision {
            VetoDecision::Deny { reason } => {
                assert_eq!(
                    reason, "first-deny",
                    "the first Deny encountered must be the aggregated reason",
                );
            }
            VetoDecision::Allow => panic!("multiple Deny hooks must aggregate to Deny"),
        }
        assert_eq!(first_deny.load(Ordering::SeqCst), 1);
        assert_eq!(
            second_deny.load(Ordering::SeqCst),
            1,
            "subsequent hooks must still observe the transition even after a Deny",
        );
    }

    #[tokio::test]
    async fn register_pre_cleanup_hook_returns_running_count() {
        let registry = HookRegistry::new();
        let invocations = Arc::new(AtomicUsize::new(0));
        let count_after_first = registry.register_pre_cleanup_hook(Arc::new(AllowingHook {
            invocations: invocations.clone(),
        }));
        let count_after_second = registry.register_pre_cleanup_hook(Arc::new(AllowingHook {
            invocations: invocations.clone(),
        }));
        assert_eq!(count_after_first, 1);
        assert_eq!(count_after_second, 2);
        assert_eq!(registry.len(), 2);
    }
}
