//! Observability HTTP API per fr:10.
pub mod log_layer;
pub mod projection;
pub mod routes;
pub mod sanitize;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::cache::DiffCache;
use crate::escalation::EscalationQueue;
use crate::events::EventWriter;
use crate::linear::polling::NudgeHandle;
use crate::observability::EventRing;

pub struct ApiState {
    pub cache: Arc<DiffCache>,
    pub workflow: Arc<WorkflowConfig>,
    pub cfg: Arc<RokiConfig>,
    pub escalation: Arc<EscalationQueue>,
    pub ring: Arc<EventRing>,
    pub nudge: NudgeHandle,
    pub request_counter: Arc<AtomicU64>,
    pub boot_time: OffsetDateTime,
    pub daemon_writer: Arc<Mutex<EventWriter>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiBindError {
    #[error("bind {bind}:{port} failed: {source}")]
    Bind {
        bind: String,
        port: u16,
        #[source]
        source: std::io::Error,
    },
}

pub async fn serve(state: Arc<ApiState>) -> Result<(), ApiBindError> {
    let bind = state.cfg.api.bind.clone();
    let port = state.cfg.api.port.expect("serve called with port unset");
    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .expect("validated bind addr");
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| ApiBindError::Bind {
            bind: bind.clone(),
            port,
            source: e,
        })?;
    let app = routes::build_router(state.clone());
    let _ = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await;
    Ok(())
}
