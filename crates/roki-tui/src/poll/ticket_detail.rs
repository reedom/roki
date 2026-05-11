use std::sync::Arc;
use std::time::Duration;

use roki_api_types::CycleSummary;
use tokio::sync::{mpsc, watch};
use tokio::time;

use crate::client::ApiClient;
use crate::model::{PollSource, Update};

pub async fn run(
    client: Arc<ApiClient>,
    cadence_seconds: u32,
    mut focus_rx: watch::Receiver<Option<String>>,
    tx: mpsc::Sender<Update>,
) {
    let mut interval = time::interval(Duration::from_secs(cadence_seconds as u64));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = focus_rx.changed() => {}
            _ = interval.tick() => {}
        }
        let Some(ticket_id) = focus_rx.borrow().clone() else {
            continue;
        };
        if !poll_once(&client, &ticket_id, &tx).await {
            return;
        }
    }
}

/// Returns `false` when the receiver has closed and the task should exit.
async fn poll_once(client: &ApiClient, ticket_id: &str, tx: &mpsc::Sender<Update>) -> bool {
    if !forward_detail(client, ticket_id, tx).await {
        return false;
    }
    forward_cycles_and_tail(client, ticket_id, tx).await
}

async fn forward_detail(client: &ApiClient, ticket_id: &str, tx: &mpsc::Sender<Update>) -> bool {
    match client.fetch_ticket_detail(ticket_id).await {
        Ok(d) => tx.send(Update::TicketDetail(d)).await.is_ok(),
        Err(e) => send_error(tx, PollSource::TicketDetail, e.to_string()).await,
    }
}

async fn forward_cycles_and_tail(
    client: &ApiClient,
    ticket_id: &str,
    tx: &mpsc::Sender<Update>,
) -> bool {
    let rows = match client.fetch_cycles(ticket_id).await {
        Ok(rows) => rows,
        Err(e) => return send_error(tx, PollSource::Cycles, e.to_string()).await,
    };
    let target = latest_cycle(&rows).cloned();
    if tx.send(Update::Cycles(rows)).await.is_err() {
        return false;
    }
    let Some(c) = target else {
        return true;
    };
    let Some(state_id) = c.last_state_id.clone() else {
        return true;
    };
    if c.total_visits == 0 {
        return true;
    }
    match client
        .fetch_visit_stdout(ticket_id, c.cycle_id, c.total_visits, &state_id)
        .await
    {
        Ok(body) => tx
            .send(Update::Tail {
                visit_n: c.total_visits,
                body,
            })
            .await
            .is_ok(),
        Err(e) => send_error(tx, PollSource::Tail, e.to_string()).await,
    }
}

fn latest_cycle(rows: &[CycleSummary]) -> Option<&CycleSummary> {
    rows.iter().max_by_key(|c| c.started_at)
}

async fn send_error(tx: &mpsc::Sender<Update>, source: PollSource, message: String) -> bool {
    tx.send(Update::PollError { source, message }).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::time::OffsetDateTime;
    use roki_api_types::{CycleKind, CycleTrigger, TicketDetail, TicketSummary};
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cycle_at(secs: i64, last_state_id: Option<&str>, visits: u32) -> CycleSummary {
        CycleSummary {
            cycle_id: Uuid::new_v4(),
            kind: CycleKind::Rule,
            trigger: CycleTrigger::Runtime,
            started_at: OffsetDateTime::from_unix_timestamp(secs).unwrap(),
            ended_at: None,
            terminal_id: None,
            failure_kind: None,
            last_state_id: last_state_id.map(str::to_string),
            total_visits: visits,
        }
    }

    fn ticket_detail() -> TicketDetail {
        TicketDetail {
            summary: TicketSummary {
                ticket_id: "ENG-1".into(),
                repo: "x/y".into(),
                status: "open".into(),
                labels: vec![],
                assignee: "u".into(),
                in_flight_cycle_id: None,
                last_event_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            },
            recent_events: vec![],
            truncated: false,
        }
    }

    #[test]
    fn latest_cycle_picks_max_started_at() {
        let rows = vec![
            cycle_at(100, Some("a"), 1),
            cycle_at(300, Some("b"), 2),
            cycle_at(200, Some("c"), 3),
        ];
        let picked = latest_cycle(&rows).unwrap();
        assert_eq!(picked.last_state_id.as_deref(), Some("b"));
    }

    #[test]
    fn latest_cycle_is_none_on_empty() {
        let rows: Vec<CycleSummary> = vec![];
        assert!(latest_cycle(&rows).is_none());
    }

    async fn mount_detail_and_cycles(srv: &MockServer, cycles: Vec<CycleSummary>) {
        Mock::given(method("GET"))
            .and(path("/api/tickets/ENG-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ticket_detail()))
            .mount(srv)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tickets/ENG-1/cycles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(cycles))
            .mount(srv)
            .await;
    }

    async fn collect_updates(rx: &mut mpsc::Receiver<Update>, max: usize) -> Vec<Update> {
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while out.len() < max && std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(u)) => out.push(u),
                _ => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn poll_once_fetches_tail_only_when_last_state_id_and_visits_present() {
        let srv = MockServer::start().await;
        let cyc = cycle_at(100, Some("post0"), 1);
        let cycle_id = cyc.cycle_id;
        mount_detail_and_cycles(&srv, vec![cyc]).await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/tickets/ENG-1/cycles/{cycle_id}/visits/1/post0/stdout"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string("tail-body"))
            .mount(&srv)
            .await;

        let client = ApiClient::new(&srv.uri()).unwrap();
        let (tx, mut rx) = mpsc::channel::<Update>(8);
        assert!(poll_once(&client, "ENG-1", &tx).await);
        drop(tx);
        let updates = collect_updates(&mut rx, 8).await;
        assert!(matches!(updates[0], Update::TicketDetail(_)));
        assert!(matches!(updates[1], Update::Cycles(_)));
        match &updates[2] {
            Update::Tail { visit_n, body } => {
                assert_eq!(*visit_n, 1);
                assert_eq!(body, "tail-body");
            }
            other => panic!("expected Tail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_once_skips_tail_when_last_state_id_missing() {
        let srv = MockServer::start().await;
        mount_detail_and_cycles(&srv, vec![cycle_at(100, None, 3)]).await;
        let client = ApiClient::new(&srv.uri()).unwrap();
        let (tx, mut rx) = mpsc::channel::<Update>(8);
        assert!(poll_once(&client, "ENG-1", &tx).await);
        drop(tx);
        let updates = collect_updates(&mut rx, 8).await;
        assert_eq!(updates.len(), 2);
        assert!(matches!(updates[0], Update::TicketDetail(_)));
        assert!(matches!(updates[1], Update::Cycles(_)));
    }

    #[tokio::test]
    async fn poll_once_skips_tail_when_total_visits_zero() {
        let srv = MockServer::start().await;
        mount_detail_and_cycles(&srv, vec![cycle_at(100, Some("post0"), 0)]).await;
        let client = ApiClient::new(&srv.uri()).unwrap();
        let (tx, mut rx) = mpsc::channel::<Update>(8);
        assert!(poll_once(&client, "ENG-1", &tx).await);
        drop(tx);
        let updates = collect_updates(&mut rx, 8).await;
        assert_eq!(updates.len(), 2);
        assert!(matches!(updates[1], Update::Cycles(_)));
    }

    #[tokio::test]
    async fn poll_once_emits_cycles_pollsource_on_cycles_failure() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tickets/ENG-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ticket_detail()))
            .mount(&srv)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tickets/ENG-1/cycles"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
            .mount(&srv)
            .await;
        let client = ApiClient::new(&srv.uri()).unwrap();
        let (tx, mut rx) = mpsc::channel::<Update>(8);
        assert!(poll_once(&client, "ENG-1", &tx).await);
        drop(tx);
        let updates = collect_updates(&mut rx, 8).await;
        assert!(matches!(updates[0], Update::TicketDetail(_)));
        match &updates[1] {
            Update::PollError { source, .. } => assert_eq!(*source, PollSource::Cycles),
            other => panic!("expected PollError(Cycles), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_once_emits_tail_pollsource_on_stdout_failure() {
        let srv = MockServer::start().await;
        let cyc = cycle_at(100, Some("post0"), 1);
        let cycle_id = cyc.cycle_id;
        mount_detail_and_cycles(&srv, vec![cyc]).await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/tickets/ENG-1/cycles/{cycle_id}/visits/1/post0/stdout"
            )))
            .respond_with(ResponseTemplate::new(503).set_body_string("nope"))
            .mount(&srv)
            .await;
        let client = ApiClient::new(&srv.uri()).unwrap();
        let (tx, mut rx) = mpsc::channel::<Update>(8);
        assert!(poll_once(&client, "ENG-1", &tx).await);
        drop(tx);
        let updates = collect_updates(&mut rx, 8).await;
        let last = updates.last().unwrap();
        match last {
            Update::PollError { source, .. } => assert_eq!(*source, PollSource::Tail),
            other => panic!("expected PollError(Tail), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_once_returns_false_when_receiver_closed() {
        let srv = MockServer::start().await;
        mount_detail_and_cycles(&srv, vec![]).await;
        let client = ApiClient::new(&srv.uri()).unwrap();
        let (tx, rx) = mpsc::channel::<Update>(8);
        drop(rx);
        assert!(!poll_once(&client, "ENG-1", &tx).await);
    }
}
