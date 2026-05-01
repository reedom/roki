//! Tracker → orchestrator bridge (task 3.6).
//!
//! Both delivery paths surfaced by Requirement 3.1 — the webhook hot path
//! (`tracker::webhook`) and the polling fallback (`tracker::linear`) —
//! produce [`NormalizedIssue`] events on a `tokio::sync::mpsc::Sender`. The
//! orchestrator core (task 3.2) accepts a single `mpsc::Receiver<NormalizedIssue>`
//! as its `tracker_inbox`. The [`TrackerBridge`] is the seam that fans both
//! sources into that one inbox while enforcing the deduplication rule
//! design.md pins:
//!
//! > Risks: webhook duplicate delivery. Mitigation: orchestrator transitions
//! > are idempotent on `(repo, issue, target_state)`.
//! > — design.md, Implementation Notes for the TrackerAdapter
//!
//! The orchestrator already treats illegal `(previous, next)` pairs as
//! no-ops, but a duplicate event still wakes a per-actor task and produces
//! observability noise. The bridge keeps the orchestrator's transition
//! stream noiseless by short-circuiting at the source: only forward a
//! `NormalizedIssue` when its `(repo, issue, state)` triple differs from
//! the most recently forwarded triple for the same `(repo, issue)` key.
//!
//! ## Idempotence model
//!
//! The bridge maintains a per-`IssueId` "last forwarded state" map. An
//! incoming event is forwarded iff:
//!
//! * the issue has never been forwarded (first observation), OR
//! * the incoming `state` differs from the last forwarded `state` for the
//!   issue (a real transition).
//!
//! This is a transition-based dedup: a re-poll of the same state (or a
//! webhook re-delivery) collapses to a no-op without ever waking the
//! orchestrator's actor for the issue. State changes still propagate.
//!
//! Task 7.1b collapsed the dedup key from `(repo, issue, target_state)` to
//! `(issue, target_state)`. The incoming `NormalizedIssue.repo` field is
//! ignored here; repo association moves onto the (post-7.1d)
//! `WorktreeRegistry`, which is per-worker rather than per-tracker-event.
//!
//! ## Linear writes are forbidden here
//!
//! Per Requirement 3.5, the daemon never issues Linear write operations
//! from within its own process — every write originates from the agent
//! through the `linear_graphql` proxy tool. The bridge has no Linear API
//! surface (no reqwest client, no token, no GraphQL types). It moves
//! [`NormalizedIssue`] values between channels and nothing else. This is
//! enforced by construction: the only inputs are two
//! `mpsc::Receiver<NormalizedIssue>` and the only output is one
//! `mpsc::Sender<NormalizedIssue>`. A reviewer (or a future change) cannot
//! sneak a Linear write into this module without adding a new dependency
//! and a new field to the struct, both of which are visible in diff.
//!
//! ## Shutdown
//!
//! The bridge exits cleanly when both input channels close. The output
//! channel is dropped along with the [`TrackerBridge`] value so the
//! orchestrator's `tracker_inbox.recv()` resolves with `None` once the
//! bridge has fully drained.

use std::collections::HashMap;

use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::orchestrator::state::IssueId;
use crate::tracker::model::{IssueState, NormalizedIssue};

/// Merge polling and webhook [`NormalizedIssue`] streams into the
/// orchestrator's tracker-event sink with `(issue, target_state)`
/// idempotence.
///
/// Construct with [`TrackerBridge::new`]; drive with [`TrackerBridge::run`]
/// from a tokio task. The future resolves when both input channels close,
/// at which point the output sender is dropped and the orchestrator's
/// `tracker_inbox` will see `None`.
pub struct TrackerBridge {
    polling: Option<mpsc::Receiver<NormalizedIssue>>,
    webhook: Option<mpsc::Receiver<NormalizedIssue>>,
    out: mpsc::Sender<NormalizedIssue>,
    last_forwarded: HashMap<IssueId, IssueState>,
}

impl TrackerBridge {
    /// Build a bridge that fans `polling` and `webhook` into `out`.
    ///
    /// Both inputs are consumed; the bridge owns them for the duration of
    /// [`Self::run`]. The `out` sender feeds the orchestrator's
    /// `tracker_inbox` (see [`crate::orchestrator::core::Orchestrator::new`]).
    pub fn new(
        polling: mpsc::Receiver<NormalizedIssue>,
        webhook: mpsc::Receiver<NormalizedIssue>,
        out: mpsc::Sender<NormalizedIssue>,
    ) -> Self {
        Self {
            polling: Some(polling),
            webhook: Some(webhook),
            out,
            last_forwarded: HashMap::new(),
        }
    }

    /// Drive the bridge until both inputs close.
    ///
    /// Each received event is dispatched through [`Self::dispatch`], which
    /// applies the dedup rule and forwards survivors to the orchestrator
    /// inbox. When both input channels return `None` the loop exits and
    /// the bridge releases its `out` sender, signalling shutdown to the
    /// orchestrator.
    pub async fn run(mut self) {
        loop {
            // `recv()` on `None` would panic; we keep each input in an
            // Option and detach the receiver permanently when its channel
            // closes. The select stays alive as long as at least one input
            // remains.
            match (self.polling.as_mut(), self.webhook.as_mut()) {
                (Some(poll_rx), Some(web_rx)) => {
                    tokio::select! {
                        biased;
                        maybe_event = poll_rx.recv() => {
                            match maybe_event {
                                Some(event) => self.dispatch(event).await,
                                None => {
                                    debug!(target: "tracker_bridge", "polling input closed");
                                    self.polling = None;
                                }
                            }
                        }
                        maybe_event = web_rx.recv() => {
                            match maybe_event {
                                Some(event) => self.dispatch(event).await,
                                None => {
                                    debug!(target: "tracker_bridge", "webhook input closed");
                                    self.webhook = None;
                                }
                            }
                        }
                    }
                }
                (Some(poll_rx), None) => match poll_rx.recv().await {
                    Some(event) => self.dispatch(event).await,
                    None => {
                        debug!(target: "tracker_bridge", "polling input closed; bridge exiting");
                        self.polling = None;
                        break;
                    }
                },
                (None, Some(web_rx)) => match web_rx.recv().await {
                    Some(event) => self.dispatch(event).await,
                    None => {
                        debug!(target: "tracker_bridge", "webhook input closed; bridge exiting");
                        self.webhook = None;
                        break;
                    }
                },
                (None, None) => break,
            }
        }
    }

    /// Apply the `(issue, target_state)` dedup rule and forward survivors to
    /// the orchestrator inbox.
    async fn dispatch(&mut self, event: NormalizedIssue) {
        let key = event.issue.clone();
        let incoming_state = event.state;

        if let Some(last) = self.last_forwarded.get(&key)
            && *last == incoming_state
        {
            // Same target state already observed — drop. This is the
            // dedup branch the design pins: orchestrator transitions are
            // idempotent on (issue, target_state) post-7.1b.
            trace!(
                target: "tracker_bridge",
                issue = %key.as_str(),
                state = ?incoming_state,
                "duplicate (issue, state); dropping",
            );
            return;
        }

        if self.out.send(event).await.is_err() {
            // Orchestrator's tracker_inbox closed — this is a shutdown
            // race. Drop the event silently and let the bridge wind down
            // naturally when its inputs close.
            debug!(target: "tracker_bridge", "orchestrator inbox closed; bridge will exit");
            return;
        }

        self.last_forwarded.insert(key, incoming_state);
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the dedup logic. End-to-end fan-in coverage that
    //! includes the webhook + polling concurrency surface lives in the
    //! integration test at `tests/tracker_bridge.rs`.

    use super::*;
    use crate::orchestrator::state::RepoId;

    fn ev(repo: &str, issue: &str, state: IssueState) -> NormalizedIssue {
        NormalizedIssue {
            repo: RepoId::new(repo),
            issue: IssueId::new(issue),
            title: String::new(),
            description: String::new(),
            state,
            labels: Vec::new(),
            team_or_scope: "ENG".to_string(),
        }
    }

    #[tokio::test]
    async fn first_observation_is_forwarded() {
        let (poll_tx, poll_rx) = mpsc::channel(4);
        let (web_tx, web_rx) = mpsc::channel(4);
        let (out_tx, mut out_rx) = mpsc::channel(4);

        let handle = tokio::spawn(TrackerBridge::new(poll_rx, web_rx, out_tx).run());

        let event = ev("repo-a", "ENG-1", IssueState::Active);
        poll_tx.send(event.clone()).await.unwrap();
        drop(poll_tx);
        drop(web_tx);

        assert_eq!(out_rx.recv().await, Some(event));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_state_is_dropped() {
        let (poll_tx, poll_rx) = mpsc::channel(4);
        let (web_tx, web_rx) = mpsc::channel(4);
        let (out_tx, mut out_rx) = mpsc::channel(4);

        let handle = tokio::spawn(TrackerBridge::new(poll_rx, web_rx, out_tx).run());

        let event = ev("repo-a", "ENG-1", IssueState::Active);
        poll_tx.send(event.clone()).await.unwrap();
        poll_tx.send(event.clone()).await.unwrap();
        drop(poll_tx);
        drop(web_tx);

        assert_eq!(out_rx.recv().await, Some(event));
        assert!(out_rx.recv().await.is_none(), "duplicate must drop");
        handle.await.unwrap();
    }
}
