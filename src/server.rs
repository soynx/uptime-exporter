//! HTTP server exposing `/metrics` (Prometheus text format) and `/healthz`.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::sync::watch;
use tracing::{error, info};

use crate::metrics::Metrics;

pub async fn serve(
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    info!(%addr, "serving /metrics");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .context("metrics server failed")
}

async fn metrics_handler(State(metrics): State<Arc<Metrics>>) -> Response {
    match metrics.encode() {
        Ok((body, content_type)) => {
            ([(header::CONTENT_TYPE, content_type)], body).into_response()
        }
        Err(e) => {
            error!(error = %e, "failed to encode metrics");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to encode metrics: {e}"),
            )
                .into_response()
        }
    }
}
