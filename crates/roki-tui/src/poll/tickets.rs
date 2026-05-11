use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;

use crate::client::ApiClient;
use crate::model::{PollSource, Update};

pub async fn run(client: Arc<ApiClient>, cadence_seconds: u32, tx: mpsc::Sender<Update>) {
    let mut interval = time::interval(Duration::from_secs(cadence_seconds as u64));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match client.fetch_tickets().await {
            Ok(rows) => {
                if tx.send(Update::Tickets(rows)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                if tx
                    .send(Update::PollError {
                        source: PollSource::Tickets,
                        message: e.to_string(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}
