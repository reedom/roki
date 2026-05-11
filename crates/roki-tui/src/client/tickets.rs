use roki_api_types::{TicketDetail, TicketSummary};

use super::{ApiClient, ClientError, get_json};

impl ApiClient {
    pub async fn fetch_tickets(&self) -> Result<Vec<TicketSummary>, ClientError> {
        let url = self.url("api/tickets")?;
        get_json(&self.http, url).await
    }

    pub async fn fetch_ticket_detail(&self, id: &str) -> Result<TicketDetail, ClientError> {
        let url = self.url(&format!("api/tickets/{id}"))?;
        get_json(&self.http, url).await
    }
}
