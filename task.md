# Tasks & milestones

## Milestone 1 — scaffold & submodule
- [x] Add `codex` submodule (`git submodule add`).
- [x] Root `Cargo.toml` workspace (`crates/*`), `justfile`, `.gitmodules`.

## Milestone 2 — cross-cutting capabilities
- [x] `aria-agent-memo`: `MemoStore` (sled) — `memorize`/`recall`/`compact`/`get_by_key`;
      `ContextFragment` with `key` (keyed long-term memory) + `FragmentKind`
      (`Message`/`ToolResult`/`LongTerm`/`Note`); `RecallQuery` (`session`/`text`/
      `top_k`/`kind`); local hashing-trick `LocalEmbedder` + `cosine` for semantic
      (vector) recall (auto-populated `embedding`, key match ranks first). Unit
      tests cover normal + abnormal paths (empty query, `compact` on missing
      session → `NotFound`, embedder on empty text).
- [x] `aria-agent-sandbox`: `Sandbox` trait + Docker (default) / Kata / Cube providers.
- [x] `aria-agent-core`: `Agent` run loop (`recall → model → memorize`), `ModelClient`
      (`StubModel` + `OpenAiModel` behind `openai`), `exec_tool` via sandbox.

## Milestone 3 — SDK (FFI)
- [x] `ariacompute-agent`: UniFFI 0.28.3 `cdylib`, exported `SdkAgent` / `SdkSession` / `create_agent`.
- [x] `aria-agent-ffigen`: regenerates Swift/Kotlin bindings (pinned to 0.28.3).

## Milestone 4 — cloud
- [x] `aria-agent-cloud`: axum + Postgres (metadata) + OpenAI; `/v1/agents`,
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
- [x] `aria-agent-reef`: `RecordStore` + `FeedbackStore` (sled) with eligibility logic.
- [x] `aria-agent-reef`: `Harness` Markdown read/write + `git.rs` versioning
      (init / commit / tag `reef@<n>` / list, fail-closed when git unavailable).
- [x] `aria-agent-reef`: `EvolutionEngine` — propose candidate via the pluggable
      local model engine (`ModelClient`, "local engine FFI"), score against the
      baseline, keep only the winner (no regression), persist + version, and
      atomically hot-swap the shared `ActiveHarness`.
- [x] `aria-agent-core`: `Harness` / `Skill` / `Rule` types + `ActiveHarness`
      (`Arc<RwLock<Arc<Harness>>>`) hot-swappable handle; `Agent` consumes the
      active harness (`with_harness`, `harness()`); system prompt assembled from
      the harness (`system_text`).
- [x] `aria-agent-cloud`: record every turn (`x-reef-agent-record-id` incl. SSE),
      `POST /reef/report`, `POST /reef/evolve`, `GET /reef/versions`; load last
      winning harness on boot (fallback to baseline), share across all agents.
- [x] `docs/adr/0006-reef-self-improvement.md`; `justfile` `reef-test` / `cov`.
- [x] Cross-crate integration test `tests/integration.rs` (no API key / Postgres);
      `cargo test --workspace`, clippy `-D warnings`, fmt all green.

## Milestone 8 — Release & publishing workflows
- [x] `.github/workflows/release.yml`: on `release: created`, build/test/`clippy` and
      package the `aria-agent-cloud` binary + `ariacompute-agent` FFI cdylib (`libaria-agent_ffi`) across
      linux-x86_64 / windows-x86_64 / linux-arm64 / macos; embed release tag via
      `ARIA_AGENT_VERSION`; upload assets with `softprops/action-gh-release`
      (`secrets.ARIACOMPUTE_TOKEN`).
- [x] `release.yml` `publish-packages` job: fail-pass (`continue-on-error: true`),
      stubs crates.io / CocoaPods Swift (`bindings/swift/AriaAgent.podspec`) / Maven
      Central so language packages never block CLI/FFI assets.
- [x] `.github/workflows/publish-cargo.yml`: topological crates.io publish of
      `aria-agent-memo` → `aria-agent-sandbox` → `aria-agent-core` → `ariacompute-agent` with version
      injection + registry-lag/429 retries (`secrets.CARGO_REGISTRY_TOKEN`).
- [x] `.github/workflows/publish-maven.yml`: publish the Kotlin/Android binding to
      Maven Central via vanniktech (`secrets.SONATYPE_*` + `secrets.GPG_*`).
- [x] `bindings/kotlin/ariacompute-agent/build.gradle.kts`: apply `com.vanniktech.maven.publish`
      with `publishToMavenCentral(CENTRAL_PORTAL, automaticRelease)` +
      `signAllPublications()` + in-memory GPG signing.
- [x] `bindings/swift/AriaAgent.podspec`: CocoaPods spec wrapping `libaria-agent_ffi` for Swift.

## Milestone 9 — OpenAI-compatible streaming & default agent
- [x] `aria-agent-cloud`: replace the bare-token SSE with a frozen
      **OpenAI Responses API** event envelope via
      `crates/aria-agent-cloud/src/event_envelope.rs` (`response.created` /
      `response.in_progress` / `response.output_item.added` /
      `response.function_call_arguments.delta` / `response.output_text.delta` /
      `response.output_text.done` / `response.output_item.done` /
      `response.completed` / `response.failed`, monotonic `sequence_number`);
      `response.completed` is the terminal event and is always the last event
      (`ensure_closed` guarantees it), with a trailing `data: [DONE]` frame
      kept only for OpenAI wire compatibility. No private `aria.*` frames —
      phase, tool result and `reef_record_id` travel as extra fields inside
      OpenAI-shaped frames. Unit tests assert the mapping, that no private
      frame leaks, and that the last frame is always terminal.
- [x] `aria-agent-cloud`: `run_agent_stream` drives
      `Agent::run_event_stream(input, agent_tools())` with a `shell` tool,
      executed by the codex sandbox backend (`sandbox_provider = "codex"`,
      ADR-0005).
- [x] `aria-agent-cloud`: `GET /v1/agents` list (admins see all, tenants their
      own) + idempotent default agent seed `agent-demo` / `Agent Demo` with an
      in-place rename of the legacy `playground-demo` row.
- [x] `aria-agent-core`: `OpenAiModel` honours `OPENAI_BASE_URL` so the agent
      can target an OpenAI-compatible gateway.
- [x] Docs: `README.md` / `README_cn.md` streaming examples (Python / Rust /
      TypeScript) parse `response.*`, plus a new "OpenAI Agents API
      compatibility" section (event table + official-SDK snippet) and the
      default-agent note; `bindings/swift/README.md` and
      `bindings/kotlin/agent-sdk/README.md` gain Cloud streaming examples.
- [x] `cargo test --workspace`, `cargo fmt --check` all green.

## Open follow-ups
* Wire real codex `sandboxing` / `linux-sandbox` / `memories` crates as precise
  path dependencies for deeper integration.
* Expose `record` / `report` via `ariacompute-agent` (UniFFI) — currently deferred to
  avoid breaking the stable FFI surface.
* Surface `AgentEvent` through the native SDKs (Swift / Kotlin) so in-process
  runs render the same timeline as the cloud stream.
