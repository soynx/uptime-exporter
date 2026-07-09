//! uptime-exporter — HTTP uptime & response-time exporter for Prometheus.
//!
//! Probes a configured list of services over HTTP(S) — deliberately *not* ICMP:
//! behind a Host-header-routed reverse proxy on a wildcard domain, only a real
//! HTTP request can tell whether a specific service (not just the box) is up.

mod config;
mod metrics;
mod probe;
mod scheduler;
mod server;

use std::sync::Arc;

use anyhow::Context;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::config::Settings;
use crate::metrics::Metrics;
use crate::probe::Prober;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let settings = Settings::from_env().context("failed to read environment settings")?;
    let services = config::load_services(&settings)?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %settings.config_path.display(),
        services = services.len(),
        "starting uptime-exporter"
    );

    let metrics = Arc::new(Metrics::new().context("failed to build metrics registry")?);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut probers = JoinSet::new();
    for service in services {
        metrics.initialize_service(&service);
        info!(
            service = %service.name,
            url = %service.url,
            interval_s = service.interval.as_secs(),
            resolve_override = ?service.resolve_override,
            "scheduling probes"
        );
        let prober = Prober::new(service)?;
        probers.spawn(scheduler::run_prober(
            prober,
            Arc::clone(&metrics),
            shutdown_rx.clone(),
        ));
    }

    // Translate SIGTERM (docker stop) / Ctrl-C into the shutdown signal.
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        info!("shutdown signal received, draining");
        let _ = shutdown_tx.send(true);
    });

    // Serve until the shutdown signal fires, then wait for probers to drain.
    let result = server::serve(settings.listen_addr, Arc::clone(&metrics), shutdown_rx).await;
    if let Err(e) = &result {
        error!(error = %e, "metrics server exited with error");
    }
    while probers.join_next().await.is_some() {}
    info!("shutdown complete");
    result
}

async fn wait_for_shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
