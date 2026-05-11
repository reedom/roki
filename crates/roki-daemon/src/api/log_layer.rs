use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::events::{Event, EventWriter, now_rfc3339};

pub struct LogState {
    pub counter: Arc<AtomicU64>,
    pub daemon_writer: Arc<Mutex<EventWriter>>,
}

pub async fn log_layer(
    State(state): State<Arc<LogState>>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query_keys: Vec<String> = request
        .uri()
        .query()
        .map(|q| {
            q.split('&')
                .filter_map(|kv| kv.split('=').next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    state.counter.fetch_add(1, Ordering::Relaxed);
    let correlation_id = Uuid::new_v4().to_string();

    let response = next.run(request).await;
    let status = response.status().as_u16();
    let duration_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;

    let mut w = state.daemon_writer.lock().await;
    let _ = w.emit(&Event::ApiRequest {
        ts: now_rfc3339(),
        method,
        path,
        query_keys,
        status,
        duration_ms,
        client_addr: client_addr.to_string(),
        correlation_id,
    });
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventWriter;
    use tempfile::TempDir;

    #[tokio::test]
    async fn log_state_counter_starts_at_zero() {
        let dir = TempDir::new().unwrap();
        let writer = EventWriter::open(dir.path(), "_daemon").unwrap();
        let state = Arc::new(LogState {
            counter: Arc::new(AtomicU64::new(0)),
            daemon_writer: Arc::new(Mutex::new(writer)),
        });
        assert_eq!(state.counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn log_state_counter_increments() {
        let dir = TempDir::new().unwrap();
        let writer = EventWriter::open(dir.path(), "_daemon").unwrap();
        let state = Arc::new(LogState {
            counter: Arc::new(AtomicU64::new(0)),
            daemon_writer: Arc::new(Mutex::new(writer)),
        });
        state.counter.fetch_add(1, Ordering::Relaxed);
        state.counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(state.counter.load(Ordering::Relaxed), 2);
    }
}
