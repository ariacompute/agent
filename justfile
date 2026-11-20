# Agent platform task runner.
# Usage: just <recipe>

egress-proxy-image-name := "aria-egress-proxy:local"

# Build all rust crates.
build:
    cargo build --workspace

# Full pre-merge gate: build + clippy + tests.
check: build
    cargo clippy --workspace --all-targets
    cargo test --workspace

# Run all rust tests.
test:
    cargo test --workspace

# Real-socket E2E suite. DB/Docker scenarios skip when their dependency is
# absent; set DATABASE_URL for the Postgres case and ARIA_E2E_DOCKER=1 for the

# Docker case (requires a cached alpine:latest image).
e2e:
    cargo test -p agent-tests --test e2e_http

# DeepSeek Harness: spawn a scripted fake `dsh` and verify the JSON-RPC

# handshake + turn framing (offline, no credentials).
dsh-smoke:
    cargo test -p aria-agent-dsh --test stdio -- --nocapture

# DeepSeek Harness live turn. Requires ARIA_DSH_E2E=1, a real `dsh` binary
# (DSH_BIN) and the same GATEWAY_BASE_URL/GATEWAY_API_KEY used by `native`
# (GATEWAY_MODEL optionally pins the model). Skips loudly when unset.
dsh-e2e:
    ARIA_DSH_E2E=1 cargo test -p aria-agent-dsh --test e2e_live -- --nocapture

# Coverage report (writes lcov.info; no gate).
cov:
    cargo llvm-cov --workspace --lcov --output-path lcov.info

# Coverage gate: 100% line coverage outside the documented exclusions.
cov-check:
    ./scripts/coverage.sh

# Generate UniFFI bindings for Swift and Kotlin from the ariacompute-agent crate.
ffi:
    cargo build -p ariacompute-agent
    cargo run -p aria-agent-ffigen

# Run the cloud service (requires Postgres + pgvector; set DATABASE_URL).
cloud:
    cargo run -p aria-agent-cloud

# Run the JS SDK tests (bun).
sdk-js-test:
    cd sdk/js && bun test

# Run the Python SDK tests (stdlib unittest).
sdk-py-test:
    cd sdk/python && python3 -m unittest discover -s tests

fmt:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets

# Apply Postgres migrations for the cloud metadata database.
migrate:
    sqlx database create
    sqlx migrate run

# Egress proxy image (network enforcement sidecar, ADR-0024).

# The build context is the repo root because the crate is a workspace member.
egress-proxy-image:
    docker build -f crates/aria-agent-egress-proxy/Dockerfile -t {{ egress-proxy-image-name }} .

# Build the proxy binary only (fast local check, no image).
egress-proxy-build:
    cargo build --release -p aria-agent-egress-proxy
