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
      `/v1/sessions/:id/runs` (JSON), `/v1/sessions/:id/runs/stream` (SSE). `migrations/0001_init.sql`.

## Milestone 5 — mobile bindings
- [x] `bindings/swift` (SwiftPM `Package.swift` + FFI modulemap).
- [x] `bindings/kotlin` (Android `build.gradle.kts` + jniLibs wiring).

## Milestone 6 — docs, tests, CI
- [x] `docs/architecture.md` + `docs/adr/*` (submodule, FFI, sandbox, memo).
- [x] Cross-crate integration tests in `tests/`.
- [x] CI workflow (build / test / fmt / clippy / ffi drift check).
- [x] Fill `README.md`, `README_cn.md`, `AGENTS.md`, `requirements.md`, `task.md`.

## Milestone 7 — Reef self-improvement  **(REMOVED)**
The whole feature was removed (crate, routes, receipt header, hot-swappable
harness). The system prompt is now static (agent name + optional `instructions`);
see `docs/followups/*` for the downstream cleanups.

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
      `response.completed` (or `response.failed`) is the only terminal event and
      is always the last event (`ensure_closed` guarantees it); there is no
      trailing `data: [DONE]` frame. No private `aria.*` frames —
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

## Milestone 9 — Context storage: Postgres + pgvector (cloud), embedded (device)
- [x] `aria-agent-core::context`: shared contract — `ContextStore`,
      `ContextFragment`, `FragmentKind`, `RecallQuery`, `ContextError`,
      `LocalEmbedder` (`EMBED_DIM = 256`), `MemoryContextStore`, and the
      `rank` / `score_fragment` / `keyword_score` blending helpers.
- [x] `aria-agent-memo`: reduced to the on-device sled store
      (`SledContextStore`) implementing the core contract (memo → core).
- [x] `aria-agent-cloud::context`: `PgContextStore` (pgvector KNN + keyword
      candidates, `principal_id` scoping) + `ensure_pgvector` boot check
      (hard failure when the `vector` extension is missing).
- [x] `migrations/0004_pgvector_context.sql` (`context_fragments`,
      `vector(256)`, HNSW `vector_cosine_ops`, tenant/session btree).
- [x] compose image → `pgvector/pgvector:pg16`; `AGENT_MEMO_BACKEND` /
      `MEMO_DIR` / `REEF_DIR` and the `memodata` / `reefdata` volumes removed.

## Milestone 10 — Reef removal
- [x] Deleted `crates/aria-agent-reef` and every reference (workspace, cloud
      state, routes, tests, justfile, compose, ADR-0006).
- [x] `aria-agent-core`: removed `Harness` / `Skill` / `Rule` / `ActiveHarness`;
      `Agent` builds via `with_model` / `with_sandbox` and serves a static
      system prompt (`system_text`, plus `AgentConfig.instructions`).
- [x] Removed `x-reef-agent-record-id` and `metadata.reef_record_id`.

## Milestone 11 — OpenAI beta Agents API (strict switch)
- [x] `api/agents.rs`: agents CRUD; `api/sessions.rs`: sessions CRUD + events +
      events/stream + read-only items / turns / subagents; `api/stubs.rs`:
      501 for vaults / environments / artifacts.
- [x] `types.rs`: OpenAI-shaped wire types (`Agent`, `AgentSession`,
      `SessionTurn`, `SessionItem`, `ListResponse`, input events).
- [x] `migrations/0005_agent_sessions.sql` (agent columns, `agent_sessions`,
      `session_turns`, `session_items`).
- [x] `event_envelope.rs` rewritten to `agent.turn.*` frames (single terminal
      event, no `[DONE]`).
- [x] Old `/v1/sessions*` routes and the `response.*` envelope removed.

## Milestone 12 — JS / Python SDKs
- [x] `sdk/js` (`@ariacompute/agent`): `Agent`, `run`, `runStreamed`, `tool`,
      `Session`; tests via `bun test`.
- [x] `sdk/python` (`ariacompute-agent`): `Agent`, `Runner.run`,
      `Runner.run_streamed`, `function_tool`, `Session`; tests via `unittest`.
- [x] CI: `publish-npm.yml`, `publish-pypi.yml`; `publish-cargo.yml` order
      updated to sandbox → core → memo → ariacompute-agent.

## Milestone 13 — SDK memory backend switch (cloud / local / both)
- [x] `aria-agent-core::context`: `MemoryBackend` (cloud | local | both),
      `CompositeContextStore` (double write + merged/deduped reads),
      `merge_fragments`, and `ContextStore::list_session`.
- [x] Deleted the sled implementation: `aria-agent-memo` is now
      `MemoContextStore` (aria memo / SQLite via rusqlite, same `memories`
      schema / `memo_type` / embedding BLOB as the `aria-memo` CLI).
- [x] `ariacompute-agent`: `CloudContextStore` (reqwest → cloud memory REST),
      `SdkMemoryConfig` on `SdkAgentConfig`, `backend` override on
      `SdkSession.memorize/recall`, `just ffi` bindings regenerated.
- [x] JS SDK: `memory` / `memoBin` / `memoDb` config, `LocalMemoryStore`
      (aria-memo CLI, injectable exec), `CompositeMemoryStore`.
- [x] Python SDK: `memory_backend` / `memo_db`, `LocalMemoryStore` (stdlib
      sqlite3, aria memo schema), `CloudMemoryStore`, `CompositeMemoryStore`.
- [x] Cloud memory endpoints accept unkeyed fragments + `kind`, and
      `GET …/memory` lists the session when no `text` is given.
- [x] Docs: `docs/adr/0011-memory-backend-switch.md`, README/README_cn five
      language examples, AGENTS.md rule 1, requirements.md §3/§5.

## Open follow-ups
* Wire real codex `sandboxing` / `linux-sandbox` / `memories` crates as precise
  path dependencies for deeper integration.
* Surface `AgentEvent` through the native SDKs (Swift / Kotlin) so in-process
  runs render the same timeline as the cloud stream.
* Downstream migrations (not in this repo): `docs/followups/playground-migration.md`,
  `docs/followups/serve-copy-migration.md`, `docs/followups/cockpit-client-migration.md`.
