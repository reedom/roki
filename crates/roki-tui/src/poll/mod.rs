//! Three independent cadences plus an on-demand ticket-detail loop. All four
//! tasks send `Update` messages into a single `mpsc::Sender<Update>` so the
//! render loop has one ordering point.

pub mod escalations;
pub mod events;
pub mod ticket_detail;
pub mod tickets;

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::client::ApiClient;
use crate::config::PollingSection;
use crate::model::Update;

pub struct PollHandles {
    pub focus_tx: watch::Sender<Option<String>>,
}

pub fn spawn(client: Arc<ApiClient>, cfg: PollingSection, tx: mpsc::Sender<Update>) -> PollHandles {
    let (focus_tx, focus_rx) = watch::channel(None);
    tokio::spawn(tickets::run(
        client.clone(),
        cfg.tickets_seconds,
        tx.clone(),
    ));
    tokio::spawn(events::run(client.clone(), cfg.events_seconds, tx.clone()));
    tokio::spawn(escalations::run(
        client.clone(),
        cfg.escalations_seconds,
        tx.clone(),
    ));
    tokio::spawn(ticket_detail::run(
        client,
        cfg.tickets_seconds,
        focus_rx,
        tx,
    ));
    PollHandles { focus_tx }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::client::ApiClient;
    use crate::model::Update;

    // Real-time sleep rather than `start_paused = true` + `tokio::time::advance`:
    // the paused-clock variant did not reliably wake the wiremock-backed poll
    // task under tokio 1.x. A short real-time wait is deterministic.
    #[tokio::test]
    async fn tickets_poll_fires_on_cadence() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tickets"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json::<Vec<roki_api_types::TicketSummary>>(vec![]),
            )
            .mount(&srv)
            .await;
        let client = Arc::new(ApiClient::new(&srv.uri()).unwrap());
        let (tx, mut rx) = mpsc::channel::<Update>(8);
        let handle = tokio::spawn(super::tickets::run(client, 1, tx));
        let mut saw_update = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(Update::Tickets(_))) => {
                    saw_update = true;
                    break;
                }
                _ => continue,
            }
        }
        handle.abort();
        assert!(saw_update, "expected at least one Tickets update within 3s");
    }
}
