use std::sync::Arc;
use std::time::Duration;

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
        let Some(ticket_id) = focus_rx.borrow().clone() else { continue };
        let detail = client.fetch_ticket_detail(&ticket_id).await;
        let cycles = client.fetch_cycles(&ticket_id).await;
        match detail {
            Ok(d) => {
                if tx.send(Update::TicketDetail(d)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Update::PollError {
                        source: PollSource::TicketDetail,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
        match cycles {
            Ok(rows) => {
                let mut latest = rows.clone();
                latest.sort_by(|a, b| b.started_at.cmp(&a.started_at));
                let target = latest.into_iter().next();
                if tx.send(Update::Cycles(rows)).await.is_err() {
                    return;
                }
                if let Some(c) = target {
                    if let (Some(state_id), n) = (c.last_state_id.clone(), c.total_visits) {
                        if n > 0 {
                            match client
                                .fetch_visit_stdout(&ticket_id, c.cycle_id, n, &state_id)
                                .await
                            {
                                Ok(body) => {
                                    let _ = tx
                                        .send(Update::Tail { visit_n: n, body })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(Update::PollError {
                                            source: PollSource::TicketDetail,
                                            message: e.to_string(),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Update::PollError {
                        source: PollSource::TicketDetail,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }
}
