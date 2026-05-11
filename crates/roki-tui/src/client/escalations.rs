use roki_api_types::ApiEscalation;

use super::{get_json, ApiClient, ClientError};

impl ApiClient {
    pub async fn fetch_escalations(&self) -> Result<Vec<ApiEscalation>, ClientError> {
        let url = self.url("api/escalations")?;
        get_json(&self.http, url).await
    }
}
