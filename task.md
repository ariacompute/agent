# Tasks & milestones

## Milestone 1 — scaffold & submodule
- [x] Add `codex` submodule (`git submodule add`).
- [x] Root `Cargo.toml` workspace (`crates/*`), `justfile`, `.gitmodules`.

## Milestone 2 — cross-cutting capabilities
- [x] `agent-memo`: `MemoStore` (sled) — `memorize`/`recall`/`compact`/`get_by_key`;
      `ContextFragment` with `key` (keyed long-term memory) + `FragmentKind`
      (`Message`/`ToolResult`/`LongTerm`/`Note`); `RecallQuery` (`session`/`text`/
      `top_k`/`kind`); local hashing-trick `LocalEmbedder` + `cosine` for semantic
      (vector) recall (auto-populated `embedding`, key match ranks first). Unit
      tests cover normal + abnormal paths (empty query, `compact` on missing
      session → `NotFound`, embedder on empty text).
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

## Milestone 7 — Reef self-improvement
- [x] `agent-reef`: `RecordStore` + `FeedbackStore` (sled) with eligibility logic.
- [x] `agent-reef`: `Harness` Markdown read/write + `git.rs` versioning
      (init / commit / tag `reef@<n>` / list, fail-closed when git unavailable).
- [x] `agent-reef`: `EvolutionEngine` — propose candidate via the pluggable
      local model engine (`ModelClient`, "local engine FFI"), score against the
      baseline, keep only the winner (no regression), persist + version, and
      atomically hot-swap the shared `ActiveHarness`.
- [x] `agent-core`: `Harness` / `Skill` / `Rule` types + `ActiveHarness`
      (`Arc<RwLock<Arc<Harness>>>`) hot-swappable handle; `Agent` consumes the
      active harness (`with_harness`, `harness()`); system prompt assembled from
      the harness (`system_text`).
- [x] `agent-cloud`: record every turn (`x-reef-agent-record-id` incl. SSE),
      `POST /reef/report`, `POST /reef/evolve`, `GET /reef/versions`; load last
      winning harness on boot (fallback to baseline), share across all agents.
- [x] `docs/adr/0006-reef-self-improvement.md`; `justfile` `reef-test` / `cov`.
- [x] Cross-crate integration test `tests/integration.rs` (no API key / Postgres);
      `cargo test --workspace`, clippy `-D warnings`, fmt all green.

## Open follow-ups
* Wire real codex `sandboxing` / `linux-sandbox` / `memories` crates as precise
  path dependencies for deeper integration.
* Add authn/z to the cloud API and structured run streaming tokens.
* Expose `record` / `report` via `agent-sdk` (UniFFI) — currently deferred to
  avoid breaking the stable FFI surface.
