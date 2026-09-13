# Tasks & milestones

## Milestone 1 — scaffold & submodule
- [x] Add `codex` submodule (`git submodule add`).
- [x] Root `Cargo.toml` workspace (`crates/*`), `justfile`, `.gitmodules`.

## Milestone 2 — cross-cutting capabilities
- [x] `agent-memo`: `MemoStore` (sled), `memorize`/`recall`/`compact`, keyed long-term memory.
- [x] `agent-sandbox`: `Sandbox` trait + Docker (default) / Kata / Cube providers.
- [x] `agent-core`: `Agent` run loop (`recall → model → memorize`), `ModelClient`
      (`StubModel` + `OpenAiModel` behind `openai`), `exec_tool` via sandbox.

## Milestone 3 — SDK (FFI)
- [x] `agent-sdk`: UniFFI 0.28.3 `cdylib`, exported `SdkAgent` / `SdkSession` / `create_agent`.
- [x] `agent-ffigen`: regenerates Swift/Kotlin bindings (pinned to 0.28.3).

## Milestone 4 — cloud
- [x] `agent-cloud`: axum + Postgres (metadata) + OpenAI; `/v1/agents`,
      `/v1/runs` (JSON), `/v1/runs/stream` (SSE). `migrations/0001_init.sql`.

## Milestone 5 — mobile bindings
- [x] `bindings/swift` (SwiftPM `Package.swift` + FFI modulemap).
- [x] `bindings/kotlin` (Android `build.gradle.kts` + jniLibs wiring).

## Milestone 6 — docs, tests, CI
- [x] `docs/architecture.md` + `docs/adr/*` (submodule, FFI, sandbox, memo).
- [x] Cross-crate integration tests in `tests/`.
- [x] CI workflow (build / test / fmt / clippy / ffi drift check).
- [x] Fill `README.md`, `README_cn.md`, `AGENTS.md`, `requirements.md`, `task.md`.

## Open follow-ups
* Wire real codex `sandboxing` / `linux-sandbox` / `memories` crates as precise
  path dependencies for deeper integration.
* Add vector embeddings to `agent-memo` `recall` for semantic retrieval.
* Add authn/z to the cloud API and structured run streaming tokens.
