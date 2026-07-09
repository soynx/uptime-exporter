//! Configuration loading and validation.
//!
//! Two sources, merged in this order of precedence (highest first):
//! 1. Environment variables (`UPTIME_*`) — runtime/connection settings
//! 2. YAML config file — the list of services to probe plus per-service defaults
//!
//! Everything is validated fail-fast at startup: a malformed config aborts the
//! process with a precise error instead of silently probing garbage.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use reqwest::{Method, Url};
use serde::Deserialize;
use thiserror::Error;

/// Built-in fallbacks, used when neither the service nor `defaults:` sets a value.
const DEFAULT_INTERVAL_SECONDS: u64 = 30;
const DEFAULT_TIMEOUT_SECONDS: u64 = 10;
const DEFAULT_METHOD: &str = "GET";
const DEFAULT_ACCEPTABLE_STATUS: (u16, u16) = (200, 399);
const DEFAULT_FOLLOW_REDIRECTS: bool = true;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/uptime-exporter/config.yaml";
pub const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:9184";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        source: serde_yaml_ng::Error,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("invalid environment variable {name}={value}: {reason}")]
    InvalidEnv {
        name: String,
        value: String,
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Raw (serde) shapes — everything optional so defaults can be merged.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    services: Vec<RawService>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    interval_seconds: Option<u64>,
    timeout_seconds: Option<u64>,
    method: Option<String>,
    acceptable_status: Option<Vec<(u16, u16)>>,
    follow_redirects: Option<bool>,
    resolve_override: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    name: String,
    url: String,
    interval_seconds: Option<u64>,
    timeout_seconds: Option<u64>,
    method: Option<String>,
    acceptable_status: Option<Vec<(u16, u16)>>,
    follow_redirects: Option<bool>,
    resolve_override: Option<String>,
}

// ---------------------------------------------------------------------------
// Resolved, validated shapes.
// ---------------------------------------------------------------------------

/// Runtime settings sourced from the environment.
#[derive(Debug, Clone)]
pub struct Settings {
    pub config_path: PathBuf,
    pub listen_addr: SocketAddr,
    /// Global resolve override. `None` = not set (use the config file's value);
    /// `Some(None)` = explicitly disabled via an empty env var;
    /// `Some(Some(addr))` = force this override for all services without their own.
    pub resolve_override: Option<Option<SocketAddr>>,
}

impl Settings {
    pub fn from_env() -> Result<Self, ConfigError> {
        let config_path = std::env::var("UPTIME_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH));

        let listen_raw =
            std::env::var("UPTIME_LISTEN_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.into());
        let listen_addr: SocketAddr =
            listen_raw
                .parse()
                .map_err(|e| ConfigError::InvalidEnv {
                    name: "UPTIME_LISTEN_ADDR".into(),
                    value: listen_raw.clone(),
                    reason: format!("not a valid socket address: {e}"),
                })?;

        let resolve_override = match std::env::var("UPTIME_RESOLVE_OVERRIDE") {
            Ok(raw) => Some(parse_resolve_override(&raw).map_err(|reason| {
                ConfigError::InvalidEnv {
                    name: "UPTIME_RESOLVE_OVERRIDE".into(),
                    value: raw,
                    reason,
                }
            })?),
            Err(_) => None,
        };

        Ok(Self {
            config_path,
            listen_addr,
            resolve_override,
        })
    }
}

/// A single, fully-resolved service to probe.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub name: String,
    pub url: Url,
    pub interval: Duration,
    pub timeout: Duration,
    pub method: Method,
    /// Inclusive `(low, high)` HTTP status ranges considered "up".
    pub acceptable_status: Vec<(u16, u16)>,
    pub follow_redirects: bool,
    /// Pin DNS resolution of this URL's host to a fixed address (curl `--resolve`
    /// equivalent). The URL's port always wins over the port given here.
    pub resolve_override: Option<SocketAddr>,
}

impl ServiceConfig {
    pub fn is_acceptable(&self, status: u16) -> bool {
        self.acceptable_status
            .iter()
            .any(|(lo, hi)| (*lo..=*hi).contains(&status))
    }
}

/// Parse a resolve-override string: empty → `None`, `IP` or `IP:PORT` → `Some(addr)`.
/// A bare IP gets port 0, which reqwest treats as "use the URL's scheme port".
fn parse_resolve_override(raw: &str) -> Result<Option<SocketAddr>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok(Some(addr));
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(Some(SocketAddr::new(ip, 0)));
    }
    Err(format!(
        "expected an IP or IP:PORT (e.g. 192.168.1.10 or 192.168.1.10:443), got {raw:?}"
    ))
}

fn parse_method(raw: &str) -> Result<Method, String> {
    Method::from_bytes(raw.trim().to_ascii_uppercase().as_bytes())
        .map_err(|_| format!("invalid HTTP method {raw:?}"))
}

fn validate_status_ranges(name: &str, ranges: &[(u16, u16)]) -> Result<(), ConfigError> {
    if ranges.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "service {name:?}: acceptable_status must not be empty"
        )));
    }
    for &(lo, hi) in ranges {
        if lo > hi || lo < 100 || hi > 599 {
            return Err(ConfigError::Invalid(format!(
                "service {name:?}: invalid status range [{lo}, {hi}] (need 100 <= low <= high <= 599)"
            )));
        }
    }
    Ok(())
}

/// Load and validate the config file, applying env-level overrides from `settings`.
pub fn load_services(settings: &Settings) -> Result<Vec<ServiceConfig>, ConfigError> {
    let path = &settings.config_path;
    let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let raw: RawConfig =
        serde_yaml_ng::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
    resolve_services(raw, settings)
}

fn resolve_services(
    raw: RawConfig,
    settings: &Settings,
) -> Result<Vec<ServiceConfig>, ConfigError> {
    if raw.services.is_empty() {
        return Err(ConfigError::Invalid(
            "no services configured — add at least one entry under `services:`".into(),
        ));
    }

    // Env wins over the file's `defaults.resolve_override`.
    let default_resolve: Option<SocketAddr> = match &settings.resolve_override {
        Some(env_value) => *env_value,
        None => match &raw.defaults.resolve_override {
            Some(s) => parse_resolve_override(s)
                .map_err(|e| ConfigError::Invalid(format!("defaults.resolve_override: {e}")))?,
            None => None,
        },
    };

    let default_method = parse_method(raw.defaults.method.as_deref().unwrap_or(DEFAULT_METHOD))
        .map_err(|e| ConfigError::Invalid(format!("defaults.method: {e}")))?;
    let default_ranges = raw
        .defaults
        .acceptable_status
        .unwrap_or_else(|| vec![DEFAULT_ACCEPTABLE_STATUS]);
    validate_status_ranges("<defaults>", &default_ranges)?;

    let mut seen_names: HashSet<String> = HashSet::new();
    let mut services = Vec::with_capacity(raw.services.len());

    for svc in raw.services {
        let name = svc.name.trim().to_string();
        if name.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "service with url {:?} has an empty name",
                svc.url
            )));
        }
        if !seen_names.insert(name.clone()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate service name {name:?} — names must be unique (they are Prometheus labels)"
            )));
        }

        let url = Url::parse(&svc.url).map_err(|e| {
            ConfigError::Invalid(format!("service {name:?}: invalid url {:?}: {e}", svc.url))
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ConfigError::Invalid(format!(
                "service {name:?}: url scheme must be http or https, got {:?}",
                url.scheme()
            )));
        }
        if url.host_str().is_none() {
            return Err(ConfigError::Invalid(format!(
                "service {name:?}: url {:?} has no host",
                svc.url
            )));
        }

        let interval_seconds = svc
            .interval_seconds
            .or(raw.defaults.interval_seconds)
            .unwrap_or(DEFAULT_INTERVAL_SECONDS);
        if interval_seconds == 0 {
            return Err(ConfigError::Invalid(format!(
                "service {name:?}: interval_seconds must be > 0"
            )));
        }
        let timeout_seconds = svc
            .timeout_seconds
            .or(raw.defaults.timeout_seconds)
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        if timeout_seconds == 0 {
            return Err(ConfigError::Invalid(format!(
                "service {name:?}: timeout_seconds must be > 0"
            )));
        }

        let method = match &svc.method {
            Some(m) => parse_method(m)
                .map_err(|e| ConfigError::Invalid(format!("service {name:?}: {e}")))?,
            None => default_method.clone(),
        };

        let acceptable_status = match svc.acceptable_status {
            Some(ranges) => {
                validate_status_ranges(&name, &ranges)?;
                ranges
            }
            None => default_ranges.clone(),
        };

        // Per-service override wins over the (env-or-file) default.
        let resolve_override = match &svc.resolve_override {
            Some(s) => parse_resolve_override(s).map_err(|e| {
                ConfigError::Invalid(format!("service {name:?}: resolve_override: {e}"))
            })?,
            None => default_resolve,
        };

        services.push(ServiceConfig {
            name,
            url,
            interval: Duration::from_secs(interval_seconds),
            timeout: Duration::from_secs(timeout_seconds),
            method,
            acceptable_status,
            follow_redirects: svc
                .follow_redirects
                .or(raw.defaults.follow_redirects)
                .unwrap_or(DEFAULT_FOLLOW_REDIRECTS),
            resolve_override,
        });
    }

    Ok(services)
}

/// Test-only path that skips reading a file. Used by unit tests below.
#[cfg(test)]
fn services_from_str(
    yaml: &str,
    settings: &Settings,
) -> Result<Vec<ServiceConfig>, ConfigError> {
    let raw: RawConfig = serde_yaml_ng::from_str(yaml).map_err(|source| ConfigError::Parse {
        path: "<inline>".into(),
        source,
    })?;
    resolve_services(raw, settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> Settings {
        Settings {
            config_path: PathBuf::from("/dev/null"),
            listen_addr: "127.0.0.1:9184".parse().unwrap(),
            resolve_override: None,
        }
    }

    #[test]
    fn full_config_with_defaults_and_overrides() {
        let yaml = r#"
defaults:
  interval_seconds: 30
  timeout_seconds: 10
  acceptable_status: [[200, 399]]
  resolve_override: "192.168.1.10:443"
services:
  - { name: portainer, url: https://portainer.example.com }
  - name: adguard
    url: https://adguard.example.com
    interval_seconds: 60
    acceptable_status: [[200, 299], [401, 401]]
    resolve_override: ""
"#;
        let svcs = services_from_str(yaml, &test_settings()).unwrap();
        assert_eq!(svcs.len(), 2);

        let p = &svcs[0];
        assert_eq!(p.name, "portainer");
        assert_eq!(p.interval, Duration::from_secs(30));
        assert_eq!(p.method, Method::GET);
        assert!(p.follow_redirects);
        assert_eq!(
            p.resolve_override,
            Some("192.168.1.10:443".parse().unwrap())
        );
        assert!(p.is_acceptable(200));
        assert!(p.is_acceptable(302));
        assert!(!p.is_acceptable(503));

        let a = &svcs[1];
        assert_eq!(a.interval, Duration::from_secs(60));
        // Explicit empty string disables the inherited override.
        assert_eq!(a.resolve_override, None);
        assert!(a.is_acceptable(401));
        assert!(!a.is_acceptable(302));
    }

    #[test]
    fn env_resolve_override_wins_over_file_default() {
        let yaml = r#"
defaults:
  resolve_override: "10.0.0.1:443"
services:
  - { name: a, url: https://a.example.com }
"#;
        let mut settings = test_settings();
        settings.resolve_override = Some(Some("192.168.5.5:443".parse().unwrap()));
        let svcs = services_from_str(yaml, &settings).unwrap();
        assert_eq!(
            svcs[0].resolve_override,
            Some("192.168.5.5:443".parse().unwrap())
        );

        // Explicitly disabled via empty env var.
        settings.resolve_override = Some(None);
        let svcs = services_from_str(yaml, &settings).unwrap();
        assert_eq!(svcs[0].resolve_override, None);
    }

    #[test]
    fn rejects_duplicate_names() {
        let yaml = r#"
services:
  - { name: a, url: https://a.example.com }
  - { name: a, url: https://b.example.com }
"#;
        let err = services_from_str(yaml, &test_settings()).unwrap_err();
        assert!(err.to_string().contains("duplicate service name"));
    }

    #[test]
    fn rejects_bad_url_scheme_zero_interval_and_bad_range() {
        for (yaml, needle) in [
            (
                "services: [{ name: a, url: \"ftp://a.example.com\" }]",
                "scheme",
            ),
            (
                "services: [{ name: a, url: \"https://a.example.com\", interval_seconds: 0 }]",
                "interval_seconds",
            ),
            (
                "services: [{ name: a, url: \"https://a.example.com\", acceptable_status: [[399, 200]] }]",
                "invalid status range",
            ),
            ("services: [{ name: a, url: \"not a url\" }]", "invalid url"),
        ] {
            let err = services_from_str(yaml, &test_settings()).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} in error: {err}"
            );
        }
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = r#"
services:
  - { name: a, url: https://a.example.com, intervall_seconds: 5 }
"#;
        assert!(services_from_str(yaml, &test_settings()).is_err());
    }

    #[test]
    fn parses_resolve_override_forms() {
        assert_eq!(parse_resolve_override("").unwrap(), None);
        assert_eq!(parse_resolve_override("  ").unwrap(), None);
        assert_eq!(
            parse_resolve_override("192.168.1.10:443").unwrap(),
            Some("192.168.1.10:443".parse().unwrap())
        );
        // Bare IP → port 0 (reqwest substitutes the URL's scheme port).
        assert_eq!(
            parse_resolve_override("192.168.1.10").unwrap(),
            Some("192.168.1.10:0".parse().unwrap())
        );
        assert!(parse_resolve_override("portainer.local").is_err());
    }

    #[test]
    fn empty_services_rejected() {
        let err = services_from_str("services: []", &test_settings()).unwrap_err();
        assert!(err.to_string().contains("no services configured"));
    }
}
