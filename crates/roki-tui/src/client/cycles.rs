use roki_api_types::CycleSummary;
use uuid::Uuid;

use super::{get_json, get_text, ApiClient, ClientError};

impl ApiClient {
    pub async fn fetch_cycles(&self, id: &str) -> Result<Vec<CycleSummary>, ClientError> {
        let url = self.url(&format!("api/tickets/{id}/cycles"))?;
        get_json(&self.http, url).await
    }

    pub async fn fetch_visit_stdout(
        &self,
        id: &str,
        cycle: Uuid,
        visit_n: u32,
        state_id: &str,
    ) -> Result<String, ClientError> {
        let url = self.url(&format!(
            "api/tickets/{id}/cycles/{cycle}/visits/{visit_n}/{state_id}/stdout"
        ))?;
        get_text(&self.http, url).await
    }
}
