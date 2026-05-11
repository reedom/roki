use std::sync::Arc;
use std::time::Duration;

use roki_tui::client::ApiClient;
use roki_tui::model::{AppModel, Update};
use roki_tui::palette::Palette;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn post_refresh_ack_lands_in_status() {
    let srv = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/refresh"))
        .respond_with(ResponseTemplate::new(202).set_body_json(roki_api_types::RefreshAck {
            coalesced: false,
            earliest_fire_at: None,
            backoff_active: false,
        }))
        .mount(&srv)
        .await;

    let client = Arc::new(ApiClient::new(&srv.uri()).unwrap());
    let (tx, mut rx) = mpsc::channel::<Update>(8);
    let c = client.clone();
    tokio::spawn(async move {
        let ack = c.post_refresh().await.unwrap();
        tx.send(Update::RefreshAck(ack)).await.unwrap();
    });
    let update = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let mut model = AppModel::new(Palette::IndexedAnsi16);
    match update {
        Update::RefreshAck(ack) => model.apply_refresh_ack(ack),
        other => panic!("unexpected update: {other:?}"),
    }
    assert!(model.status.text().contains("coalesced=false"));
    assert!(model.status.text().contains("backoff_active=false"));
}
