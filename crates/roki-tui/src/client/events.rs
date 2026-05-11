use roki_api_types::EventsPage;

use super::{ApiClient, ClientError, get_json};

impl ApiClient {
    pub async fn fetch_events_since(&self, since: Option<u64>) -> Result<EventsPage, ClientError> {
        let mut url = self.url("api/events")?;
        if let Some(s) = since {
            url.query_pairs_mut().append_pair("since", &s.to_string());
        }
        get_json(&self.http, url).await
    }
}
