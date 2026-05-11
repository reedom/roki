use roki_api_types::RefreshAck;

use super::{ApiClient, ClientError};

impl ApiClient {
    pub async fn post_refresh(&self) -> Result<RefreshAck, ClientError> {
        let url = self.url("api/refresh")?;
        let resp = self.http.post(url).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(ClientError::Http {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).to_string(),
            });
        }
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
        crate::sanitize::clean_json(&mut value);
        let parsed: RefreshAck = serde_json::from_value(value)?;
        Ok(parsed)
    }
}
