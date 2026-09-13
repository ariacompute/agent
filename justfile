# Agent platform task runner.
# Usage: just <recipe>

# Initialize the codex submodule (shallow) if not present.
init:
    git submodule update --init --depth 1 codex

# Build all rust crates.
build:
    cargo build --workspace

# Run all rust tests.
test:
    cargo test --workspace

# Generate UniFFI bindings for Swift and Kotlin from the agent-sdk crate.
ffi:
    cargo build -p agent-sdk
    cargo run -p agent-ffigen

# Run the cloud service (requires Postgres; set DATABASE_URL).
cloud:
    cargo run -p agent-cloud

fmt:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets

# Apply Postgres migrations for the cloud metadata database.
migrate:
    sqlx database create
    sqlx migrate run
