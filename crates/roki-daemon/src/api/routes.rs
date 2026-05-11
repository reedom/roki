//! Axum router + per-endpoint handlers for fr:10.
//!
//! Every handler is intentionally thin: validation + projection dispatch +
//! content-type wrapping. The projection layer (`super::projection`) owns the
//! data-shape work; sanitization runs there before the JSON / NDJSON / text
//! bodies are returned.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use uuid::Uuid;

use roki_api_types::{
    ApiEscalation, EventsPage, Healthz, RefreshAck, TicketDetail, TicketSummary,
};

use crate::api::ApiState;
use crate::api::log_layer;
use crate::api::projection;

pub fn build_router(state: Arc<ApiState>) -> Router {
    let log_state = Arc::new(log_layer::LogState {
        counter: state.request_counter.clone(),
        daemon_writer: state.daemon_writer.clone(),
    });
    Router::new()
        .route("/api/healthz", get(healthz))
        .route("/api/tickets", get(list_tickets))
        .route("/api/tickets/:id", get(ticket_detail))
        .route("/api/tickets/:id/cycles", get(list_cycles))
        .route(
            "/api/tickets/:id/cycles/:cycle_id/visits/:n/:state_id/:stream",
            get(visit_stream),
        )
        .route("/api/events", get(events_page))
        .route("/api/escalations", get(list_escalations))
        .route("/api/refresh", post(refresh))
        .layer(axum::middleware::from_fn_with_state(
            log_state,
            log_layer::log_layer,
        ))
        .with_state(state)
}

fn json_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h
}

async fn healthz(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let uptime = (time::OffsetDateTime::now_utc() - state.boot_time)
        .whole_seconds()
        .max(0) as u64;
    let repos = state.workflow.admission_repos();
    let body = Healthz {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: uptime,
        configured_repositories: repos,
        api_request_count: state
            .request_counter
            .load(std::sync::atomic::Ordering::Relaxed),
    };
    (StatusCode::OK, json_headers(), Json(body))
}

async fn list_tickets(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let body: Vec<TicketSummary> = projection::tickets::list_tickets(&state.cache).await;
    (StatusCode::OK, json_headers(), Json(body))
}

async fn ticket_detail(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    if !is_ticket_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_ticket_id", "");
    }
    match projection::tickets::detail(
        &state.cache,
        &state.ring,
        &id,
        state.cfg.api.ticket_events_window as usize,
    )
    .await
    {
        Some(d) => (StatusCode::OK, json_headers(), Json::<TicketDetail>(d)).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "ticket_not_found", &id),
    }
}

async fn list_cycles(State(state): State<Arc<ApiState>>, Path(id): Path<String>) -> Response {
    if !is_ticket_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_ticket_id", "");
    }
    let (cycles, truncated) = projection::cycles::list_cycles(
        &state.cfg.paths.session_root,
        &id,
        state.cfg.api.cycle_list_window as usize,
    );
    #[derive(serde::Serialize)]
    struct Body {
        cycles: Vec<roki_api_types::CycleSummary>,
        truncated: bool,
    }
    (
        StatusCode::OK,
        json_headers(),
        Json(Body { cycles, truncated }),
    )
        .into_response()
}

async fn visit_stream(
    State(state): State<Arc<ApiState>>,
    Path((id, cycle_id, n, state_id, stream)): Path<(String, String, u32, String, String)>,
) -> Response {
    if !is_ticket_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_ticket_id", "");
    }
    let cycle_id = match Uuid::parse_str(&cycle_id) {
        Ok(u) => u,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_cycle_id", ""),
    };
    if !is_state_id(&state_id) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_state_id", "");
    }
    let stream_enum = match projection::visits::Stream::parse(&stream) {
        Some(s) => s,
        None => return error_response(StatusCode::BAD_REQUEST, "invalid_stream", ""),
    };
    if let Some(states) =
        projection::cycles::read_cycle_states(&state.cfg.paths.session_root, &id, cycle_id)
    {
        if !states.iter().any(|s| s == &state_id) {
            return error_response(
                StatusCode::NOT_FOUND,
                "state_id_not_found_in_cycle",
                &state_id,
            );
        }
    }
    let bytes = match projection::visits::read_stream(
        &state.cfg.paths.session_root,
        &id,
        cycle_id,
        n,
        &state_id,
        stream_enum,
    ) {
        Ok(b) => b,
        Err(_) => return error_response(StatusCode::NOT_FOUND, "stream_not_found", ""),
    };
    use projection::visits::Stream as S;
    match stream_enum {
        S::Stdout | S::Stderr | S::ExitCode => {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            let cleaned = strip_ansi_only(&s);
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            (StatusCode::OK, headers, cleaned).into_response()
        }
        S::Directive | S::Terminal => {
            let mut v: serde_json::Value =
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            crate::api::sanitize::clean_json(&mut v);
            let body = serde_json::to_string(&v).unwrap_or_default();
            (StatusCode::OK, json_headers(), body).into_response()
        }
        S::Events => {
            let mut out = String::new();
            let mut dropped = 0u32;
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_slice::<serde_json::Value>(line) {
                    Ok(mut v) => {
                        crate::api::sanitize::clean_json(&mut v);
                        out.push_str(&serde_json::to_string(&v).unwrap_or_default());
                        out.push('\n');
                    }
                    Err(_) => dropped += 1,
                }
            }
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if dropped > 0
                && let Ok(v) = HeaderValue::from_str(&dropped.to_string())
            {
                headers.insert("Roki-Dropped-Lines", v);
            }
            (StatusCode::OK, headers, out).into_response()
        }
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<u64>,
    kind: Option<String>,
    ticket: Option<String>,
    cycle: Option<String>,
    limit: Option<usize>,
}

async fn events_page(
    State(state): State<Arc<ApiState>>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let cycle = match q.cycle.as_deref() {
        Some(s) => match Uuid::parse_str(s) {
            Ok(u) => Some(u),
            Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_cycle_id", ""),
        },
        None => None,
    };
    let limit = q.limit.unwrap_or(200).min(1000);
    let body: EventsPage = projection::events::page(
        &state.ring,
        projection::events::EventsQuery {
            since: q.since,
            kind: q.kind.as_deref(),
            ticket: q.ticket.as_deref(),
            cycle,
            limit,
        },
    );
    (StatusCode::OK, json_headers(), Json(body)).into_response()
}

async fn list_escalations(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let body: Vec<ApiEscalation> = projection::escalations::list(&state.escalation).await;
    (StatusCode::OK, json_headers(), Json(body))
}

async fn refresh(
    State(state): State<Arc<ApiState>>,
    axum::extract::ConnectInfo(client_addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let ack: RefreshAck = state.nudge.nudge().await;
    // Mirror the ack as a structured event so operators can correlate
    // `/api/refresh` calls with downstream `polling_tick` entries. The
    // file-backed writer routes through the global ring (see
    // `EventWriter::emit`), so this surfaces in `/api/events` too.
    {
        let mut w = state.daemon_writer.lock().await;
        let _ = w.emit(&crate::events::Event::RefreshNudgeAcknowledged {
            ts: crate::events::now_rfc3339(),
            coalesced: ack.coalesced,
            backoff_active: ack.backoff_active,
            client_addr: client_addr.to_string(),
        });
    }
    (StatusCode::ACCEPTED, json_headers(), Json(ack))
}

fn error_response(status: StatusCode, code: &str, detail: &str) -> Response {
    #[derive(serde::Serialize)]
    struct Err<'a> {
        error: &'a str,
        detail: &'a str,
    }
    (
        status,
        json_headers(),
        Json(Err {
            error: code,
            detail,
        }),
    )
        .into_response()
}

fn is_ticket_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn is_state_id(s: &str) -> bool {
    is_ticket_id(s)
}

/// ANSI-strip without HTML-escape. `clean_text` HTML-escapes; for plain-text
/// streams we want the raw `<` / `>` / `&` preserved so the consumer sees what
/// the underlying tool actually printed. We round-trip the HTML entities back
/// to their literal bytes here.
fn strip_ansi_only(s: &str) -> String {
    let cleaned = crate::api::sanitize::clean_text(s);
    cleaned
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}
