use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;

use crate::client::ApiClient;
use crate::model::{PollSource, Update};

pub async fn run(client: Arc<ApiClient>, cadence_seconds: u32, tx: mpsc::Sender<Update>) {
    let mut interval = time::interval(Duration::from_secs(cadence_seconds as u64));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut last_seq: Option<u64> = None;
    loop {
        interval.tick().await;
        let requested = last_seq;
        match client.fetch_events_since(requested).await {
            Ok(page) => {
                last_seq = page.next_since.or(last_seq);
                if tx
                    .send(Update::Events { page, requested_since: requested })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Update::PollError {
                        source: PollSource::Events,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }
}
