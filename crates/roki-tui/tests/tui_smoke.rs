use std::time::Duration;

use roki_tui::app::App;
use time::OffsetDateTime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn drives_model_loop_against_mock() {
    let srv = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![
            roki_api_types::TicketSummary {
                ticket_id: "ENG-1".into(),
                repo: "github.com/x/y".into(),
                status: "in_progress".into(),
                labels: vec!["urgent".into()],
                assignee: "u".into(),
                in_flight_cycle_id: None,
                last_event_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            },
        ]))
        .mount(&srv)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/events"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(roki_api_types::EventsPage {
                events: vec![],
                gap: false,
                next_since: None,
            }),
        )
        .mount(&srv)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/escalations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json::<Vec<roki_api_types::ApiEscalation>>(vec![]),
        )
        .mount(&srv)
        .await;

    let model = App::run_for_test(&srv.uri(), 3, Duration::from_secs(6))
        .await
        .expect("run_for_test");
    assert!(
        !model.tickets.rows.is_empty(),
        "tickets snapshot must arrive"
    );
    assert_eq!(model.tickets.rows[0].ticket_id, "ENG-1");
}
