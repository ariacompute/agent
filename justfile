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

# Run only the agent-reef self-improvement crate tests.
reef-test:
    cargo test -p aria-agent-reef

# Coverage hint (needs cargo-llvm-cov or cargo-tarpaulin installed).
cov:
    cargo llvm-cov --workspace --lcov --output-path lcov.info || cargo tarpaulin --workspace --out Xml

# Generate UniFFI bindings for Swift and Kotlin from the ariacompute-agent crate.
ffi:
    cargo build -p ariacompute-agent
    cargo run -p aria-agent-ffigen

# Run the cloud service (requires Postgres; set DATABASE_URL).
cloud:
    cargo run -p aria-agent-cloud

fmt:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets

# Apply Postgres migrations for the cloud metadata database.
migrate:
    sqlx database create
    sqlx migrate run
