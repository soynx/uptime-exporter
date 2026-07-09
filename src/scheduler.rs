//! Per-service probe loop.
//!
//! Each service runs in its own tokio task on its own interval. A failing probe
//! never terminates the loop — it records `probe_success 0` and keeps going.

use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::metrics::Metrics;
use crate::probe::Prober;

/// Upper bound for the random startup jitter that de-synchronizes services
/// sharing the same interval (avoids probing everything in one burst).
const MAX_STARTUP_JITTER: Duration = Duration::from_secs(5);

pub async fn run_prober(
    prober: Prober,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    let interval = prober.service.interval;

    let jitter_bound = interval.min(MAX_STARTUP_JITTER).as_millis().max(1) as u64;
    let jitter = Duration::from_millis(rand::rng().random_range(0..jitter_bound));
    tokio::select! {
        _ = tokio::time::sleep(jitter) => {}
        _ = shutdown.changed() => return,
    }

    let mut ticker = tokio::time::interval(interval);
    // If a probe (worst case: its full timeout) overruns the interval, don't
    // fire a burst of make-up ticks afterwards — just resume the cadence.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // Track the previous result so recoveries are logged at INFO exactly once.
    let mut was_healthy: Option<bool> = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => {
                debug!(service = %prober.service.name, "prober shutting down");
                return;
            }
        }

        let outcome = prober.probe().await;
        metrics.record(&prober.service, &outcome);

        if outcome.success {
            if was_healthy == Some(false) {
                info!(service = %prober.service.name, "service recovered");
            }
        } else {
            warn!(
                service = %prober.service.name,
                url = %prober.service.url,
                status = outcome.status,
                total_ms = outcome.total.as_millis() as u64,
                error = outcome.error.as_deref().unwrap_or("unknown"),
                "probe failed"
            );
        }
        was_healthy = Some(outcome.success);
    }
}
