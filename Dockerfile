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
# ----------------------------------------------------------------------------

# ---- Build stage ----
FROM rust:1-slim AS builder

# Build essentials for native deps (sqlx/pg, openssl).
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    ca-certificates \
    git \
    && rm -rf /var/lib/apt/lists/*

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

RUN apt-get update && apt-get install -y --no-install-recommends \
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
