//! Integration tests for the tracker→orchestrator bridge (task 3.6).
//!
//! These tests exercise the bridge as a black box: two `NormalizedIssue`
//! sources (one simulating polling, one simulating webhook) feeding into a
//! single output channel that the orchestrator's tracker_inbox would
//! consume. The bridge is responsible for deduplicating on `(repo, issue,
//! target_state)` so the orchestrator never observes the same logical
//! transition twice when both delivery paths fire within one tick.
//!
//! Requirement 3.1: webhook + polling fallback feed the same orchestrator
//! event sink. Requirement 3.5: the bridge never performs Linear writes —
//! it is a forwarder, not a Linear API client.

use std::time::Duration;

use roki_daemon::orchestrator::state::{IssueId, RepoId};
use roki_daemon::orchestrator::tracker_bridge::TrackerBridge;
use roki_daemon::tracker::model::{IssueState, NormalizedIssue};
use tokio::sync::mpsc;

fn issue(repo: &str, issue: &str, state: IssueState) -> NormalizedIssue {
    NormalizedIssue {
        repo: RepoId::new(repo),
        issue: IssueId::new(issue),
        title: String::new(),
        description: String::new(),
        state,
        labels: Vec::new(),
    }
}

async fn recv_with_timeout(rx: &mut mpsc::Receiver<NormalizedIssue>) -> Option<NormalizedIssue> {
    tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .ok()
        .flatten()
}

/// Same logical update arriving via webhook and polling within one tick
/// must produce exactly one forwarded event into the orchestrator inbox.
/// This is the observable-completion criterion for task 3.6.
#[tokio::test]
async fn dedups_same_repo_issue_state_from_webhook_and_polling() {
    let (poll_tx, poll_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (web_tx, web_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<NormalizedIssue>(8);

    let bridge = TrackerBridge::new(poll_rx, web_rx, out_tx);
    let handle = tokio::spawn(bridge.run());

    let same = issue("repo-a", "ENG-1", IssueState::Active);
    poll_tx.send(same.clone()).await.expect("poll send");
    web_tx.send(same.clone()).await.expect("web send");

    let first = recv_with_timeout(&mut out_rx).await;
    assert_eq!(
        first,
        Some(same.clone()),
        "first observation must be forwarded to the orchestrator",
    );

    // No second observation — same triple, must be dropped.
    let second = recv_with_timeout(&mut out_rx).await;
    assert!(
        second.is_none(),
        "duplicate (repo, issue, state) must not produce a second forward; got {second:?}",
    );

    drop(poll_tx);
    drop(web_tx);
    handle.await.expect("bridge task");
}

/// Different `target_state` for the same `(repo, issue)` is a real
/// transition and must be forwarded.
#[tokio::test]
async fn forwards_state_change_for_same_key() {
    let (poll_tx, poll_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (web_tx, web_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<NormalizedIssue>(8);

    let handle = tokio::spawn(TrackerBridge::new(poll_rx, web_rx, out_tx).run());

    let active = issue("repo-a", "ENG-1", IssueState::Active);
    let terminal = issue("repo-a", "ENG-1", IssueState::Terminal);
    poll_tx.send(active.clone()).await.expect("send active");
    poll_tx.send(terminal.clone()).await.expect("send terminal");

    assert_eq!(recv_with_timeout(&mut out_rx).await, Some(active));
    assert_eq!(recv_with_timeout(&mut out_rx).await, Some(terminal));

    drop(poll_tx);
    drop(web_tx);
    handle.await.expect("bridge task");
}

/// Distinct `IssueId` keys are independently tracked.
///
/// Per task 7.1b the bridge dedup key collapsed from `(repo, issue,
/// target_state)` to `(issue, target_state)`. Two events with the same
/// `IssueId` but different `RepoId` are now considered the same logical
/// transition (the orchestrator state machine no longer keys by repo); only
/// the first observation is forwarded for that issue/state pair.
#[tokio::test]
async fn forwards_different_issues() {
    let (poll_tx, poll_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (web_tx, web_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<NormalizedIssue>(8);

    let handle = tokio::spawn(TrackerBridge::new(poll_rx, web_rx, out_tx).run());

    let one = issue("repo-a", "ENG-1", IssueState::Active);
    let two = issue("repo-a", "ENG-2", IssueState::Active);
    let three = issue("repo-a", "ENG-3", IssueState::Active);
    poll_tx.send(one.clone()).await.expect("send 1");
    poll_tx.send(two.clone()).await.expect("send 2");
    poll_tx.send(three.clone()).await.expect("send 3");

    let mut received = Vec::new();
    for _ in 0..3 {
        if let Some(ev) = recv_with_timeout(&mut out_rx).await {
            received.push(ev);
        }
    }
    assert_eq!(received.len(), 3, "all three distinct issues must forward");
    assert!(received.contains(&one));
    assert!(received.contains(&two));
    assert!(received.contains(&three));

    drop(poll_tx);
    drop(web_tx);
    handle.await.expect("bridge task");
}

/// Two events with the same `IssueId` but different `RepoId` are deduped to
/// one forward — task 7.1b's `(issue, target_state)` key collapse.
#[tokio::test]
async fn collapses_same_issue_across_repos() {
    let (poll_tx, poll_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (web_tx, web_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<NormalizedIssue>(8);

    let handle = tokio::spawn(TrackerBridge::new(poll_rx, web_rx, out_tx).run());

    let from_repo_a = issue("repo-a", "ENG-1", IssueState::Active);
    let from_repo_b = issue("repo-b", "ENG-1", IssueState::Active);
    poll_tx.send(from_repo_a.clone()).await.expect("send a");
    poll_tx.send(from_repo_b.clone()).await.expect("send b");

    let first = recv_with_timeout(&mut out_rx).await;
    assert_eq!(
        first,
        Some(from_repo_a),
        "first observation must be forwarded to the orchestrator",
    );
    let second = recv_with_timeout(&mut out_rx).await;
    assert!(
        second.is_none(),
        "same IssueId+state from a different repo must dedup; got {second:?}",
    );

    drop(poll_tx);
    drop(web_tx);
    handle.await.expect("bridge task");
}

/// A re-emitted same-state event from the polling source after a state
/// change must still dedup against the most recently forwarded state, so a
/// later poll that "rediscovers" `Active` after we already moved to
/// `Terminal` would still forward (different from the last emitted), but a
/// re-poll of the *same* state must not.
#[tokio::test]
async fn dedups_repeated_polls_of_same_state() {
    let (poll_tx, poll_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (web_tx, web_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<NormalizedIssue>(8);

    let handle = tokio::spawn(TrackerBridge::new(poll_rx, web_rx, out_tx).run());

    let active = issue("repo-a", "ENG-1", IssueState::Active);
    poll_tx.send(active.clone()).await.expect("send 1");
    poll_tx.send(active.clone()).await.expect("send 2");
    poll_tx.send(active.clone()).await.expect("send 3");

    assert_eq!(recv_with_timeout(&mut out_rx).await, Some(active));
    assert!(recv_with_timeout(&mut out_rx).await.is_none());

    drop(poll_tx);
    drop(web_tx);
    handle.await.expect("bridge task");
}

/// The bridge stops cleanly when both inputs close.
#[tokio::test]
async fn shuts_down_when_both_inputs_closed() {
    let (poll_tx, poll_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (web_tx, web_rx) = mpsc::channel::<NormalizedIssue>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<NormalizedIssue>(8);

    let handle = tokio::spawn(TrackerBridge::new(poll_rx, web_rx, out_tx).run());

    drop(poll_tx);
    drop(web_tx);

    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("bridge must exit when inputs close")
        .expect("bridge task");

    // Out channel should also be closed once the bridge finishes.
    assert!(out_rx.recv().await.is_none());
}
