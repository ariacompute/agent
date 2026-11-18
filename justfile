# Agent platform task runner.
# Usage: just <recipe>

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
