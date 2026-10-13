# syntax=docker/dockerfile:1

# ----------------------------------------------------------------------------
# Agent Cloud — Dockerfile
#
# Builds the `aria-agent` binary from the `aria-agent-cloud` crate and ships it
# as a lean runtime image. The cloud service needs:
#   * Postgres+pgvector (metadata: agents / runs AND context: context_fragments)
#                                                    -> DATABASE_URL
#   * OpenAI API    (model calls via agent-core)      -> OPENAI_API_KEY
#
# NOTE: conversational context lives in Postgres (`context_fragments`, pgvector),
# so the database image must provide the `vector` extension
# (`pgvector/pgvector:pg16`). The on-device SDK keeps its own embedded store.
#
# ---------------------------------------------------------------------------
# Build & run
# ---------------------------------------------------------------------------
# This image is built once and then run via docker compose. Two equivalent
# flows are supported:
#
#   1) Manual build, then compose up (reuses the pre-built image):
#        docker build \
#          --network=host \
#          --build-arg BUILD_PROXY=http://127.0.0.1:7897 \
#          --build-arg DOCKER_GID=998 \
#          -f Dockerfile -t aria-agent-cloud .
#        docker compose up
#
#   2) One-shot compose build + run:
#        export BUILD_PROXY=http://127.0.0.1:7897   # optional; empty = direct
#        export DOCKER_GID=998
#        docker compose up --build
#
# Build-time args (all optional, with defaults baked into the Dockerfile):
#   * BUILD_PROXY  HTTP(S) proxy for apt/cargo/git during the build. Empty => a
#                 direct internet connection. Use `--network=host` (or the
#                 compose `build.network: host`) so the container's 127.0.0.1
#                 reaches the host-side proxy (e.g. Clash on 127.0.0.1:7897).
#   * DOCKER_GID   GID of the host `docker` group owning /var/run/docker.sock
#                 (default 998). Lets the non-root `aria` user reach the mounted
#                 socket. Get it with `stat -c '%g' /var/run/docker.sock`.
#   * APT_MIRROR  apt archive mirror *host* (default mirrors.aliyun.com). Swap
#                 for mirrors.tuna.tsinghua.edu.cn / mirrors.ustc.edu.cn, etc.
# ---------------------------------------------------------------------------


# ---- Build stage ----
FROM rust:1-slim AS builder

# Outbound network proxy so cargo/git can reach GitHub to fetch the
# `ariacompute/codex` fork (the `codex-protocol` / `codex-sandboxing` git deps).
ARG BUILD_PROXY=

# Optional apt mirror to speed up `apt-get update` in regions where the default
# Debian archive (deb.debian.org) is slow/unreachable. Pass the mirror *host*
# only, e.g.:
#   --build-arg APT_MIRROR=mirrors.aliyun.com
# The original `deb.debian.org` host is substituted with this value, so the
# path (/debian, /debian-security) is preserved. Defaults to the Aliyun mirror.
ARG APT_MIRROR=mirrors.aliyun.com
RUN if [ -n "${APT_MIRROR}" ]; then \
        sed -i "s|deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list.d/debian.sources 2>/dev/null \
        || sed -i "s|deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list; \
    fi

ENV HTTP_PROXY=${BUILD_PROXY} \
    HTTPS_PROXY=${BUILD_PROXY} \
    http_proxy=${BUILD_PROXY} \
    https_proxy=${BUILD_PROXY}

# apt does not reliably honor HTTP_PROXY env vars, so when a proxy is configured
# we write an explicit apt config that routes BOTH `apt-get update` and
# `apt-get install` through it. When BUILD_PROXY is empty this is skipped and apt
# uses a direct connection.
RUN if [ -n "${BUILD_PROXY}" ]; then \
        printf 'Acquire::http::Proxy "%s";\nAcquire::https::Proxy "%s";\n' \
            "${BUILD_PROXY}" "${BUILD_PROXY}" > /etc/apt/apt.conf.d/95proxy; \
    fi

# Build essentials for native deps (sqlx/pg, openssl).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
        git \
    && rm -rf /var/lib/apt/lists/*

# Route git's own HTTPS fetches through the proxy only when one is configured.
# An empty proxy is a no-op (git falls back to a direct connection), so the
# build also works in environments without any proxy.
RUN if [ -n "${BUILD_PROXY}" ]; then \
        git config --global http.proxy "${BUILD_PROXY}" \
        && git config --global https.proxy "${BUILD_PROXY}"; \
    fi

# Same for cargo: it already honors HTTP_PROXY/HTTPS_PROXY env vars, but we also
# write an explicit cargo config so registry + git (codex fork) fetches go
# through BUILD_PROXY deterministically. CARGO_HOME is /usr/local/cargo on the
# rust slim image. Skipped when no proxy is configured.
RUN if [ -n "${BUILD_PROXY}" ]; then \
        mkdir -p /usr/local/cargo \
        && printf '[http]\nproxy = "%s"\n[https]\nproxy = "%s"\n' \
            "${BUILD_PROXY}" "${BUILD_PROXY}" > /usr/local/cargo/config.toml; \
    fi

WORKDIR /src

# Copy manifests + source (only workspace members needed for the cloud build).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests ./tests

# Build just the cloud binary (pulls in its deps only) in release mode.
RUN cargo build -p aria-agent-cloud --release --locked \
    && cp target/release/aria-agent /usr/local/bin/aria-agent

# ---- Runtime stage ----
FROM debian:bookworm-slim

# GID of the host `docker` group owning /var/run/docker.sock, so the non-root
# `aria` user can reach the mounted socket. Override at build time to match the
# host:  docker compose build --build-arg DOCKER_GID=$(stat -c '%g' /var/run/docker.sock)
ARG DOCKER_GID=998

# Same apt mirror substitution as the builder stage (see above).
ARG APT_MIRROR=mirrors.aliyun.com
RUN if [ -n "${APT_MIRROR}" ]; then \
        sed -i "s|deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list.d/debian.sources 2>/dev/null \
        || sed -i "s|deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list; \
    fi

# Builder proxy must be re-declared here; each FROM starts a fresh scope and the
# global --build-arg is only visible once an ARG is declared in this stage.
ARG BUILD_PROXY=
# Same explicit apt proxy config as the builder stage (see above).
RUN if [ -n "${BUILD_PROXY}" ]; then \
        printf 'Acquire::http::Proxy "%s";\nAcquire::https::Proxy "%s";\n' \
            "${BUILD_PROXY}" "${BUILD_PROXY}" > /etc/apt/apt.conf.d/95proxy; \
    fi
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Run as a non-root user, but let it talk to the mounted Docker socket by joining
# a group whose GID matches the host docker group.
RUN groupadd -g ${DOCKER_GID} docker \
    && useradd --create-home --uid 10001 --groups docker aria

WORKDIR /app

COPY --from=builder /usr/local/bin/aria-agent /usr/local/bin/aria-agent

# Context memory is Postgres-backed (`context_fragments`, pgvector) — no local
# volume is mounted any more.
ENV RUST_LOG=info

USER aria

EXPOSE 3000

# The binary binds 0.0.0.0:3000 and reads env vars at startup.
ENTRYPOINT ["aria-agent"]
