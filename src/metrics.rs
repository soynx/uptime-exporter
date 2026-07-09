//! Prometheus registry and metric families.
//!
//! Metric naming mirrors `blackbox_exporter` conventions (`probe_success`,
//! `probe_duration_seconds`, ...) under an `uptime_` prefix, so standard Grafana
//! dashboards and alert patterns translate directly.

use std::time::{SystemTime, UNIX_EPOCH};

use prometheus::{Encoder, GaugeVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder};

use crate::config::ServiceConfig;
use crate::probe::ProbeOutcome;

const LABELS: &[&str] = &["name", "url"];

pub struct Metrics {
    registry: Registry,
    /// 1 if the last probe succeeded (transport OK + acceptable status), else 0.
    probe_success: GaugeVec,
    /// Last probe timing, split by phase: `ttfb` (headers received), `total`.
    probe_duration_seconds: GaugeVec,
    /// Last HTTP status code; 0 if the probe failed before a response line.
    probe_http_status_code: GaugeVec,
    /// Unix timestamp of the last completed probe — alert on staleness to
    /// detect a stuck prober.
    probe_last_run_timestamp_seconds: GaugeVec,
    /// Monotonic probe counter by result (`success` / `fail`) for rate() alerts.
    probe_total: IntCounterVec,
}

impl Metrics {
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();

        let probe_success = GaugeVec::new(
            Opts::new(
                "uptime_probe_success",
                "Whether the last probe of the service succeeded (1) or failed (0).",
            ),
            LABELS,
        )?;
        let probe_duration_seconds = GaugeVec::new(
            Opts::new(
                "uptime_probe_duration_seconds",
                "Duration of the last probe in seconds, by phase (ttfb, total). \
                 0 for phases that were never reached.",
            ),
            &["name", "url", "phase"],
        )?;
        let probe_http_status_code = GaugeVec::new(
            Opts::new(
                "uptime_probe_http_status_code",
                "HTTP status code of the last probe response; 0 if no response was received.",
            ),
            LABELS,
        )?;
        let probe_last_run_timestamp_seconds = GaugeVec::new(
            Opts::new(
                "uptime_probe_last_run_timestamp_seconds",
                "Unix timestamp of the last completed probe for this service.",
            ),
            LABELS,
        )?;
        let probe_total = IntCounterVec::new(
            Opts::new(
                "uptime_probe_total",
                "Total number of probes performed, by result (success/fail).",
            ),
            &["name", "url", "result"],
        )?;
        let build_info = IntGaugeVec::new(
            Opts::new("uptime_build_info", "Build information of uptime-exporter."),
            &["version"],
        )?;

        registry.register(Box::new(probe_success.clone()))?;
        registry.register(Box::new(probe_duration_seconds.clone()))?;
        registry.register(Box::new(probe_http_status_code.clone()))?;
        registry.register(Box::new(probe_last_run_timestamp_seconds.clone()))?;
        registry.register(Box::new(probe_total.clone()))?;
        registry.register(Box::new(build_info.clone()))?;

        build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);

        Ok(Self {
            registry,
            probe_success,
            probe_duration_seconds,
            probe_http_status_code,
            probe_last_run_timestamp_seconds,
            probe_total,
        })
    }

    /// Pre-create all label combinations for a service so every configured
    /// service is visible in /metrics from the first scrape (counters at 0,
    /// `last_run` at 0 until the first probe lands).
    pub fn initialize_service(&self, service: &ServiceConfig) {
        let (name, url) = (service.name.as_str(), service.url.as_str());
        for result in ["success", "fail"] {
            self.probe_total.with_label_values(&[name, url, result]);
        }
        self.probe_last_run_timestamp_seconds
            .with_label_values(&[name, url]);
    }

    pub fn record(&self, service: &ServiceConfig, outcome: &ProbeOutcome) {
        let (name, url) = (service.name.as_str(), service.url.as_str());
        let labels = &[name, url];

        self.probe_success
            .with_label_values(labels)
            .set(if outcome.success { 1.0 } else { 0.0 });
        self.probe_http_status_code
            .with_label_values(labels)
            .set(f64::from(outcome.status.unwrap_or(0)));
        self.probe_duration_seconds
            .with_label_values(&[name, url, "ttfb"])
            .set(outcome.ttfb.map_or(0.0, |d| d.as_secs_f64()));
        self.probe_duration_seconds
            .with_label_values(&[name, url, "total"])
            .set(outcome.total.as_secs_f64());
        self.probe_last_run_timestamp_seconds
            .with_label_values(labels)
            .set(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64()),
            );
        self.probe_total
            .with_label_values(&[name, url, if outcome.success { "success" } else { "fail" }])
            .inc();
    }

    /// Encode the registry in Prometheus text exposition format.
    pub fn encode(&self) -> Result<(Vec<u8>, String), prometheus::Error> {
        let encoder = TextEncoder::new();
        let mut buffer = Vec::with_capacity(4096);
        encoder.encode(&self.registry.gather(), &mut buffer)?;
        Ok((buffer, encoder.format_type().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::{Method, Url};
    use std::time::Duration;

    fn svc() -> ServiceConfig {
        ServiceConfig {
            name: "grafana".into(),
            url: Url::parse("https://grafana.example.com/").unwrap(),
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
            method: Method::GET,
            acceptable_status: vec![(200, 399)],
            follow_redirects: true,
            resolve_override: None,
        }
    }

    #[test]
    fn records_and_encodes() {
        let metrics = Metrics::new().unwrap();
        let service = svc();
        metrics.initialize_service(&service);
        metrics.record(
            &service,
            &ProbeOutcome {
                success: true,
                status: Some(200),
                ttfb: Some(Duration::from_millis(120)),
                total: Duration::from_millis(300),
                error: None,
            },
        );

        let (buf, content_type) = metrics.encode().unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(content_type.starts_with("text/plain"));
        assert!(text.contains(
            "uptime_probe_success{name=\"grafana\",url=\"https://grafana.example.com/\"} 1"
        ));
        assert!(text.contains("uptime_probe_http_status_code"));
        assert!(text.contains("phase=\"ttfb\""));
        assert!(text.contains("uptime_probe_total"));
        assert!(text.contains("uptime_build_info"));

        // Failure without a response → status 0, ttfb 0.
        metrics.record(
            &service,
            &ProbeOutcome {
                success: false,
                status: None,
                ttfb: None,
                total: Duration::from_secs(10),
                error: Some("timeout".into()),
            },
        );
        let (buf, _) = metrics.encode().unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains(
            "uptime_probe_success{name=\"grafana\",url=\"https://grafana.example.com/\"} 0"
        ));
        assert!(text.contains(
            "uptime_probe_http_status_code{name=\"grafana\",url=\"https://grafana.example.com/\"} 0"
        ));
    }
}
