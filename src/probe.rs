//! A single HTTP(S) probe against one configured service.
//!
//! Design notes:
//! - **One `reqwest::Client` per service**, built once at startup. This lets each
//!   service carry its own timeout, redirect policy and DNS resolve override.
//! - **No connection pooling** (`pool_max_idle_per_host(0)`): every probe performs
//!   a full DNS → TCP → TLS handshake. Reusing connections would hide exactly the
//!   failures (and latency regressions) an uptime probe exists to detect.
//! - **Resolve override**: the Rust equivalent of `curl --resolve host:443:IP`.
//!   On a homelab that hosts the probed services itself, resolving the public
//!   wildcard record would depend on flaky NAT hairpinning — pinning the host to
//!   the reverse proxy's LAN address keeps Host/SNI (and thus Traefik routing and
//!   certificate validation) fully intact while making the network path reliable.

use std::time::{Duration, Instant};

use anyhow::Context;
use reqwest::redirect::Policy;
use reqwest::Client;
use tracing::debug;

use crate::config::ServiceConfig;

/// Result of one probe run. `total` is always the wall time until the outcome
/// was known (for a timeout that is ≈ the configured timeout — informative).
#[derive(Debug)]
pub struct ProbeOutcome {
    pub success: bool,
    /// HTTP status if a response line was received; `None` on transport failure.
    pub status: Option<u16>,
    /// Time to first byte (response headers received). `None` if we never got that far.
    pub ttfb: Option<Duration>,
    pub total: Duration,
    /// Human-readable failure reason for logging. Never used as a metric label
    /// (unbounded cardinality).
    pub error: Option<String>,
}

pub struct Prober {
    client: Client,
    pub service: ServiceConfig,
}

impl Prober {
    pub fn new(service: ServiceConfig) -> anyhow::Result<Self> {
        let mut builder = Client::builder()
            .user_agent(concat!("uptime-exporter/", env!("CARGO_PKG_VERSION")))
            // Fresh connection per probe — see module docs.
            .pool_max_idle_per_host(0)
            .timeout(service.timeout)
            .connect_timeout(service.timeout)
            .redirect(if service.follow_redirects {
                Policy::limited(10)
            } else {
                Policy::none()
            });

        if let Some(addr) = service.resolve_override {
            // Safe unwrap: config validation guarantees the URL has a host.
            let host = service
                .url
                .host_str()
                .expect("validated URL must have a host");
            // Note: this pins only this exact host. If the service redirects to a
            // *different* host, that host resolves via normal DNS.
            builder = builder.resolve(host, addr);
        }

        let client = builder
            .build()
            .with_context(|| format!("failed to build HTTP client for {:?}", service.name))?;
        Ok(Self { client, service })
    }

    pub async fn probe(&self) -> ProbeOutcome {
        let start = Instant::now();
        let response = self
            .client
            .request(self.service.method.clone(), self.service.url.clone())
            .send()
            .await;

        match response {
            Ok(mut resp) => {
                let ttfb = start.elapsed();
                let status = resp.status().as_u16();

                // Drain the body chunk-by-chunk (discarded, never buffered) so
                // `total` reflects the complete response and mid-body failures
                // (e.g. a proxy cutting the stream) count as probe failures.
                let mut body_error: Option<reqwest::Error> = None;
                loop {
                    match resp.chunk().await {
                        Ok(Some(_)) => continue,
                        Ok(None) => break,
                        Err(e) => {
                            body_error = Some(e);
                            break;
                        }
                    }
                }
                let total = start.elapsed();

                if let Some(e) = body_error {
                    return ProbeOutcome {
                        success: false,
                        status: Some(status),
                        ttfb: Some(ttfb),
                        total,
                        error: Some(format!("body read failed: {}", classify_error(&e))),
                    };
                }

                let acceptable = self.service.is_acceptable(status);
                debug!(
                    service = %self.service.name,
                    status,
                    ttfb_ms = ttfb.as_millis() as u64,
                    total_ms = total.as_millis() as u64,
                    "probe completed"
                );
                ProbeOutcome {
                    success: acceptable,
                    status: Some(status),
                    ttfb: Some(ttfb),
                    total,
                    error: (!acceptable).then(|| format!("unacceptable HTTP status {status}")),
                }
            }
            Err(e) => ProbeOutcome {
                success: false,
                status: None,
                ttfb: None,
                total: start.elapsed(),
                error: Some(classify_error(&e)),
            },
        }
    }
}

/// Prefix reqwest errors with a stable category so log lines are grep-able,
/// and append the full `source()` chain — reqwest's `Display` alone hides the
/// root cause (e.g. certificate errors, ECONNREFUSED) behind
/// "error sending request".
fn classify_error(e: &reqwest::Error) -> String {
    let category = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_redirect() {
        "redirect"
    } else if e.is_request() {
        "request"
    } else if e.is_body() || e.is_decode() {
        "body"
    } else {
        "other"
    };
    let mut message = format!("{category}: {e}");
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::{Method, Url};
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn service(url: &str, acceptable: Vec<(u16, u16)>) -> ServiceConfig {
        ServiceConfig {
            name: "test".into(),
            url: Url::parse(url).unwrap(),
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(2),
            method: Method::GET,
            acceptable_status: acceptable,
            follow_redirects: false,
            resolve_override: None,
        }
    }

    /// Minimal one-shot HTTP/1.1 fixture: accepts a single connection and
    /// answers with the given status line + body.
    async fn spawn_fixture(status_line: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                // Read the request (until headers end) so the client isn't reset early.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = "ok";
                let resp = format!(
                    "{status_line}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn probe_success_on_200() {
        let addr = spawn_fixture("HTTP/1.1 200 OK").await;
        let prober = Prober::new(service(&format!("http://{addr}/"), vec![(200, 399)])).unwrap();
        let outcome = prober.probe().await;
        assert!(outcome.success, "outcome: {outcome:?}");
        assert_eq!(outcome.status, Some(200));
        assert!(outcome.ttfb.unwrap() <= outcome.total);
    }

    #[tokio::test]
    async fn probe_fails_on_unacceptable_status() {
        let addr = spawn_fixture("HTTP/1.1 503 Service Unavailable").await;
        let prober = Prober::new(service(&format!("http://{addr}/"), vec![(200, 399)])).unwrap();
        let outcome = prober.probe().await;
        assert!(!outcome.success);
        assert_eq!(outcome.status, Some(503));
        assert!(outcome.error.unwrap().contains("503"));
    }

    #[tokio::test]
    async fn probe_fails_on_connection_refused() {
        // Bind-then-drop guarantees an unused port.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let prober = Prober::new(service(&format!("http://{addr}/"), vec![(200, 399)])).unwrap();
        let outcome = prober.probe().await;
        assert!(!outcome.success);
        assert_eq!(outcome.status, None);
        assert_eq!(outcome.ttfb, None);
        assert!(outcome.error.is_some());
    }

    #[tokio::test]
    async fn resolve_override_pins_hostname_to_fixture() {
        // URL uses a hostname that does NOT resolve publicly; the override pins
        // it to the local fixture — exactly the LAN-pinning mechanism used in prod.
        let addr = spawn_fixture("HTTP/1.1 200 OK").await;
        let mut svc = service(
            &format!("http://uptime-exporter-test.invalid:{}/", addr.port()),
            vec![(200, 399)],
        );
        svc.resolve_override = Some(addr);
        let prober = Prober::new(svc).unwrap();
        let outcome = prober.probe().await;
        assert!(outcome.success, "outcome: {outcome:?}");
        assert_eq!(outcome.status, Some(200));
    }
}
