# uptime-exporter

HTTP uptime & response-time exporter for Prometheus, built for homelabs where a
reverse proxy (Traefik) routes many services on a **wildcard domain** by Host
header (`portainer.example.com`, `grafana.example.com`, ... → one public IP).

## Quick Setup

Setup on docker:

```yaml
services:
  uptime-exporter:
    image: ghcr.io/soynx/uptime-exporter:latest
    restart: unless-stopped
    volumes:
      - /path/to/config:/etc/uptime-exporter:ro  # mount config file
    ports:
      - "9184:9184"
```
Exposes `/metrics` at port `9184` on host

## Why HTTP probes, not ping

With wildcard DNS every subdomain resolves to the **same IP**; routing to the
actual service happens purely on the **Host header / SNI** of an HTTP(S)
request. ICMP ping carries neither — it can only tell you the box is up, never
whether *portainer* specifically is up. This exporter issues a real HTTPS
request per service, so a dead backend behind a live proxy (`502`/`503`) is
correctly reported as **down**, and you get true per-service latency.

## Why the resolve override (LAN pinning)

When the exporter runs on the same host it monitors, resolving the public
wildcard record means the connection must hairpin through the router's NAT
loopback — which is unreliable on many consumer routers and can cause
false "down" alerts. `resolve_override` pins DNS for the probed hostnames to
the reverse proxy's **LAN address** (the equivalent of
`curl --resolve host:443:LAN_IP`) while keeping the real hostname as Host
header and SNI. Traefik routing and Let's Encrypt certificate validation behave
exactly as for an external visitor; only the network path is made reliable.
The trade-off — this checks the *internal* path only — is intentional: pair it
with an external uptime monitor for the WAN path.

## Metrics

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `uptime_probe_success` | gauge | `name`, `url` | 1 = last probe OK (transport + acceptable status), 0 = down |
| `uptime_probe_duration_seconds` | gauge | `name`, `url`, `phase` | Last probe timing; `phase` ∈ `ttfb`, `total` |
| `uptime_probe_http_status_code` | gauge | `name`, `url` | Last status code; 0 = no response received |
| `uptime_probe_last_run_timestamp_seconds` | gauge | `name`, `url` | Unix time of last completed probe (alert on staleness) |
| `uptime_probe_total` | counter | `name`, `url`, `result` | Probe count by `success` / `fail` |
| `uptime_build_info` | gauge | `version` | Always 1 |

Endpoints: `GET /metrics`, `GET /healthz` (default port **9184**).

## Configuration

Services come from a YAML file (see [`config.example.yaml`](config.example.yaml)):

```yaml
defaults:
  interval_seconds: 30
  timeout_seconds: 10
  method: GET
  acceptable_status: [[200, 399]]     # inclusive ranges; add [401, 401] for auth-gated roots
  follow_redirects: true
  resolve_override: "192.168.1.10:443"    # LAN pin; "" or omit = normal DNS

services:
  - name: portainer
    url: https://portainer.example.com
  - name: grafana
    url: https://grafana.example.com
    interval_seconds: 60              # every field can be overridden per service
```

Runtime settings via environment (env beats file):

| Variable | Default | Purpose |
|---|---|---|
| `UPTIME_CONFIG_PATH` | `/etc/uptime-exporter/config.yaml` | Config file location |
| `UPTIME_LISTEN_ADDR` | `0.0.0.0:9184` | `/metrics` listen address |
| `UPTIME_RESOLVE_OVERRIDE` | *(unset)* | Global override of `defaults.resolve_override` (`""` disables pinning) |
| `RUST_LOG` | `info` | Log filter (`tracing_subscriber` EnvFilter syntax) |

Notes:
- The config is validated **fail-fast** at startup (unique names, valid URLs,
  intervals > 0, sane status ranges, parseable override address). Typos in
  field names are rejected too.
- Probe interval should roughly match the Prometheus scrape interval; the
  gauges hold the *last* probe's values.
- The resolve override pins only the probed hostname. If `follow_redirects` is
  on and a service redirects to a *different* host, that host uses normal DNS.
- Every probe opens a **fresh connection** (full DNS → TCP → TLS handshake) so
  latency and cert problems are never masked by connection reuse.

## Run locally

```bash
UPTIME_CONFIG_PATH=./config.example.yaml RUST_LOG=info cargo run
curl -s localhost:9184/metrics | grep uptime_probe
```

## Docker

Tagged releases (`v*`) are built and published to GHCR by CI
([`docker-publish.yml`](.github/workflows/docker-publish.yml)) as
`ghcr.io/<owner>/uptime-exporter` with `latest`, `X.Y` and `X.Y.Z` tags.
Or build locally:

```bash
docker build -t uptime-exporter .
docker run --rm -p 9184:9184 \
  -v "$PWD/config.yaml:/etc/uptime-exporter/config.yaml:ro" \
  uptime-exporter
```

Portainer stack (attach to the network Prometheus scrapes on):

```yaml
services:
  uptime-exporter:
    image: ghcr.io/<owner>/uptime-exporter:latest
    restart: unless-stopped
    volumes:
      - /opt/uptime-exporter/config.yaml:/etc/uptime-exporter/config.yaml:ro
    networks: [monitoring]
networks:
  monitoring:
    external: true
```

## Prometheus & alerting

```yaml
scrape_configs:
  - job_name: uptime-exporter
    scrape_interval: 30s
    static_configs:
      - targets: ["uptime-exporter:9184"]
```

Suggested alerts:

```yaml
- alert: ServiceDown
  expr: uptime_probe_success == 0
  for: 2m
  annotations:
    summary: "{{ $labels.name }} is down ({{ $labels.url }})"

- alert: UptimeProberStale
  expr: time() - uptime_probe_last_run_timestamp_seconds > 300
  for: 5m
  annotations:
    summary: "prober for {{ $labels.name }} has not run in 5 minutes"
```

## Grafana dashboard

Import [`grafana-dashboard.json`](grafana-dashboard.json): **Dashboards → New →
Import → Upload JSON file**, then pick your Prometheus datasource in the
`Prometheus` dropdown (top-left variable). Includes: up/down status map,
outage timeline, availability % over the selected range, response-time and
TTFB graphs, a per-service overview table with clickable URLs, and a
failures-per-hour panel for spotting flapping services.

## Development

```bash
cargo test          # unit tests (config validation, probe fixtures, metrics)
cargo clippy --all-targets
```
