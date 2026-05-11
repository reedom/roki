//! E2E: `roki events` (online) against a wiremock server.
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_roki")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn online_dump_against_wiremock() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let body = serde_json::json!({
        "events": [{
            "seq": 1,
            "ts": "2026-05-11T10:00:00Z",
            "event": "webhook_received",
            "ticket_id": "ENG-3",
            "payload": {"foo": "bar"}
        }],
        "gap": false,
        "next_since": 2,
    });
    Mock::given(method("GET"))
        .and(path("/api/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let out = Command::new(bin())
        .args(["events", "--api", &server.uri()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("\"seq\":1"));
    assert!(s.contains("webhook_received"));
}
