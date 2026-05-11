use roki_api_types::ApiEscalation;

use super::{ApiClient, ClientError, get_json};

impl ApiClient {
    pub async fn fetch_escalations(&self) -> Result<Vec<ApiEscalation>, ClientError> {
        let url = self.url("api/escalations")?;
        get_json(&self.http, url).await
    }
}
