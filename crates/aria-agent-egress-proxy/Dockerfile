# syntax=docker/dockerfile:1
#
# Egress proxy image — the only container on the `aria-egress` internal network
# besides the sandboxes. It holds no secrets and no policy: every decision is a
# call back to the control plane (ADR-0024).
#
# Build (context = repo root, the crate is a workspace member):
#   docker build -f crates/aria-agent-egress-proxy/Dockerfile -t aria-egress-proxy .
#
# Runtime env (all required unless noted):
#   CONTROL_PLANE_URL      e.g. http://aria-agent-cloud:8080
#   EGRESS_PROXY_TOKEN     shared secret; provided by the control plane at startup
#   EGRESS_PROXY_BIND      default 0.0.0.0:8080
#   EGRESS_PROXY_HEALTH_BIND  default 0.0.0.0:8081

# ---- build stage ----
FROM rust:1-bookworm AS builder
ARG BUILD_PROXY=
ENV HTTP_PROXY=${BUILD_PROXY} \
    HTTPS_PROXY=${BUILD_PROXY} \
    http_proxy=${BUILD_PROXY} \
    https_proxy=${BUILD_PROXY}
WORKDIR /src
# Only the sources the proxy needs; the rest of the workspace is not copied so a
# change in an unrelated crate does not bust this layer.
COPY Cargo.toml Cargo.lock ./
COPY crates/aria-agent-core crates/aria-agent-core
COPY crates/aria-agent-governance crates/aria-agent-governance
COPY crates/aria-agent-sandbox crates/aria-agent-sandbox
COPY crates/aria-agent-egress-proxy crates/aria-agent-egress-proxy
COPY crates/aria-agent-plugin crates/aria-agent-plugin
COPY crates/aria-agent-tools crates/aria-agent-tools
COPY crates/aria-agent-orchestrator crates/aria-agent-orchestrator
COPY crates/aria-agent-memo crates/aria-agent-memo
COPY crates/aria-agent-dsh crates/aria-agent-dsh
COPY crates/aria-agent-browser crates/aria-agent-browser
COPY crates/ariacompute-agent crates/ariacompute-agent
COPY crates/aria-agent-cloud crates/aria-agent-cloud
COPY crates/aria-agent-ffigen crates/aria-agent-ffigen
COPY tests tests
RUN cargo build --release --locked -p aria-agent-egress-proxy

# ---- runtime stage ----
FROM debian:bookworm-slim AS runtime
# CA certificates: the proxy only needs TLS when CONTROL_PLANE_URL is https.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/aria-egress-proxy /usr/local/bin/aria-egress-proxy
# Non-root: the proxy never needs to write anything.
USER 65534:65534
EXPOSE 8080 8081
HEALTHCHECK --interval=10s --timeout=2s --retries=3 \
    CMD ["/usr/local/bin/aria-egress-proxy", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/aria-egress-proxy"]
