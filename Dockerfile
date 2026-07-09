# ---- build stage -----------------------------------------------------------
FROM rust:1-slim AS builder
WORKDIR /app

# Cache dependency compilation: build once with a dummy main, then the real src.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# touch so cargo notices the real sources are newer than the dummy build
RUN touch src/main.rs && cargo build --release --locked

# ---- runtime stage ----------------------------------------------------------
# Mozilla roots are compiled in (webpki-roots); ca-certificates additionally
# provides the host trust store so private/corporate CAs work too.
FROM debian:stable-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin uptime

COPY --from=builder /app/target/release/uptime-exporter /usr/local/bin/uptime-exporter

USER uptime
EXPOSE 9184
ENV UPTIME_CONFIG_PATH=/etc/uptime-exporter/config.yaml \
    UPTIME_LISTEN_ADDR=0.0.0.0:9184 \
    RUST_LOG=info

ENTRYPOINT ["/usr/local/bin/uptime-exporter"]
