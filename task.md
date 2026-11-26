# Tasks & milestones

## Milestone 1 — scaffold
- [x] Root `Cargo.toml` workspace (`crates/*`), `justfile`, `rust-toolchain`.

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
      stubs crates.io / CocoaPods Swift (`bindings/swift/AriaComputeAgent.podspec`) / Maven
      Central so language packages never block CLI/FFI assets.
- [x] `.github/workflows/publish-cargo.yml`: topological crates.io publish of
      `aria-agent-memo` → `aria-agent-sandbox` → `aria-agent-core` → `ariacompute-agent` with version
      injection + registry-lag/429 retries (`secrets.CARGO_REGISTRY_TOKEN`).
- [x] `.github/workflows/publish-maven.yml`: publish the Kotlin/Android binding to
      Maven Central via vanniktech (`secrets.SONATYPE_*` + `secrets.GPG_*`).
- [x] `bindings/kotlin/ariacompute-agent/build.gradle.kts`: apply `com.vanniktech.maven.publish`
      with `publishToMavenCentral(CENTRAL_PORTAL, automaticRelease)` +
      `signAllPublications()` + in-memory GPG signing.
- [x] `bindings/swift/AriaComputeAgent.podspec`: CocoaPods spec wrapping `libaria-agent_ffi` for Swift.

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
      executed by the docker sandbox backend (ADR-0003/ADR-0018).
- [x] `aria-agent-cloud`: `GET /v1/agents` list (admins see all, tenants their
      own) + idempotent default agent seed `agent-demo` / `Agent Demo` with an
      in-place rename of the legacy `playground-demo` row.
- [x] `aria-agent-core`: `OpenAiModel` honours `GATEWAY_BASE_URL` /
      `GATEWAY_API_KEY` / `GATEWAY_MODEL` so the agent can target an
      OpenAI-compatible gateway (no `OPENAI_*` fallback).
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

## Milestone 14 — SDK 成熟度（测试 + 功能对齐，参考 memo）
- [x] 类型化错误：`AriaError`（`kind ∈ {auth,network,api,config,unknown}`），
      `transport` 将 401/403→auth、其他非 2xx→api（带 status）、连接失败→network；
      js/python 双端 `transport`/`runner`/`session` 单测覆盖各 kind。
- [x] `Session` 只读端点：`getItems()`/`getTurns()`（GET `…/items`、`…/turns`），
      对应规则 8 不改动上下文；js/python 双端单测。
- [x] `maxTurns`：`run`/`runStreamed` 接受正整数上限；`runStreamed` 按
      `agent.turn.created` 计数超额早退并抛 `config` 型错误；双端单测。
- [x] `Agent` 校验：`name` 必填非空、`tools` 名称唯一（`duplicate tool name`）、
      `toolSchemas()` 不透传 `execute` 且默认 `parameters`；双端单测。
- [x] 测试硬化：js `bun test` 51 用例、python `unittest` 28 用例全绿，覆盖正常 +
      异常（类型化错误、未知 backend、`getItems`/`getTurns`、空/数组输入、
      `maxTurns` 上限、单侧 backend 失败、SSE 解码回退）。
- [x] 原生面 `ariacompute-agent` 保持稳定：新特性为云 REST 层，不映射到本地优先
      运行时，避免 FFI 绑定漂移（规则 2）；crate 编译通过。
- [x] 文档同步：AGENTS.md 新增「SDK（js/python）」章节、requirements.md §3.1 行为
      契约、本里程碑条目。

## Milestone 15 — Browser sandbox (six-image catalog + dynamic switching)
- [x] `aria-agent-browser` (new workspace crate): `BrowserKind`
      (`servo` / `obscura` / `chromium` / `gosub` / `camoufox` / `lightpanda`),
      `BrowserCatalog` (six default images + `ARIACOMPUTE_BROWSER_IMAGE_<KIND>`
      overrides + default-engine fallback), `BrowserEngine` trait, op/response
      protocol, graceful `Unsupported` degradation for native engines.
- [x] `aria-agent-browser/src/sandbox.rs`: `BrowserSandbox` implements both
      `Sandbox` and `BrowserEngine`; reuses the existing Docker/Kata bollard path
      and `SandboxResourceLimits` (`build_host_config`); `switch_to(kind, image?)`
      tears down the current container and re-pulls the chosen image with safe
      rollback (no panic / no dropped connection, no daemon ⇒ not panicking).
- [x] `aria-agent-browser/docker`: uniform `runner.mjs` driver across all six
      engines + `Dockerfile.playwright` (Chromium / Camoufox / Lightpanda base),
      `Dockerfile.obscura` (Chromium stealth layer), `Dockerfile.servo` /
      `Dockerfile.gosub` (minimal native-engine images).
- [x] `aria-agent-core`: new `ToolHandler` trait (`handles` + `run`) and
      `Agent::with_handlers`; `run_event_stream` / `exec_tool_call` route a
      `ToolCall` through registered handlers before falling back to the shell
      `sandbox_exec`. Frozen `AgentEvent` streaming contract (rule 9) unchanged.
- [x] `aria-agent-browser/src/tools.rs`: `browser_tools()` family
      (`browser_navigate` / `browser_extract` / `browser_click` / `browser_fill`
      / `browser_screenshot` / `browser_evaluate` / `browser_solve_captcha` /
      `browser_use`) + `BrowserToolHandler`; `browser_use` drives
      `BrowserSandbox::switch_to` for runtime engine switching.
- [x] `aria-agent-sandbox`: `SandboxProvider::Browser` variant (parse / as_str),
      `from_provider` returns `NotConfigured` (no heavy dep in the sandbox crate).
- [x] `aria-agent-cloud`: `build_agent` special-cases `sandbox_provider =
      "browser"` to inject `BrowserSandbox` as a `ToolHandler` while the shell
      fallback stays on Docker; `agent_tools()` appends `browser_tools()`.
- [x] `ariacompute-agent`: `sdk_tools()` appends `browser_tools()`; exported FFI
      surface unchanged (no `just ffi` regeneration needed).
- [x] `docs/adr/0012-browser-sandbox.md`: six-image catalog, container reuse,
      dynamic selection/switching, graceful degradation decision.
- [x] Tests: per-kind engine normal + abnormal paths (empty url / unreachable /
      bad selector / screenshot timeout / switch failure → rollback), sandbox
      no-daemon non-panic, `ToolHandler` routing, `browser_use` switch success +
      safe fallback; `cargo test --workspace` green.

## Milestone 16 — agent × memo full integration
- [x] `aria-agent-core/src/lib.rs`: `AgentConfig` gains `memory_graph: bool`
      (default false) + `memory_compact_threshold: usize` (default 0); `run` /
      `run_stream` / `run_event_stream` supplement vector `recall` with bounded
      `expand` (graph), relate each turn's user↔assistant with `RelationKind::
      Temporal`, and `compact` once the session exceeds the threshold. Graph/compact
      calls are best-effort (`let _ =` / `if let Ok`) so a backend without graph
      support degrades silently.
- [x] `aria-agent-core/src/tools/memo.rs`: `memo_tools()` (`memo_store` / `memo_get`
      / `memo_search` / `memo_related`) + `MemoToolHandler` (`ToolHandler`); tool
      results persisted as `ToolResult`. Re-exported at crate root.
- [x] `aria-agent-core/src/context/mod.rs` + `aria-agent-memo/src/lib.rs`:
      `compact` reuses the existing `__compact__{session}` note id (idempotent), so
      repeated compactions overwrite instead of accumulating.
- [x] `ariacompute-agent/src/lib.rs`: `make_agent` always builds via
      `Agent::with_handlers`, injecting `MemoToolHandler` (and `BrowserToolHandler`
      for `sandbox_provider = "browser"`); sets `memory_graph = true`,
      `threshold = 32`; `sdk_tools()` appends `memo_tools()`. FFI export unchanged.
- [x] `aria-agent-cloud/src/main.rs`: keep `memory_graph = false` (PgContextStore
      supports it; enable per-deployment).
- [x] `docs/adr/0013-agent-memo-integration.md`: graph memory + default backend +
      tool family decision.
- [x] Tests: `ariacompute-agent` end-to-end with a real `MemoContextStore` —
      cross-turn recall, memory-graph edges, memo tools persist/query, idempotent
      compaction; plus `sdk_tools()` asserts 4 memo tools. `cargo test --workspace`
      green; `cargo fmt` + `clippy` clean.

## eMudhra / emSign 证书私钥泄露响应（2026-10）

背景：eMudhra(emSign) 8 张中间 CA 私钥极弱（P-384 私钥标量 <256），可被反推伪造任意域名证书；
Rust(webpki-roots) / Bun / Node / Python 等自带信任库需升级。

### 1. Rust（云服务端 / 所有 Rust 二进制）
- `Cargo.lock`：`webpki-roots` 1.0.9 -> 1.0.10（`cargo update -p webpki-roots --precise 1.0.10`）。
  1.0.10+ 在 rustls 信任库中移除 emSign 根；`aria-agent-cloud` 经 reqwest+rustls-tls 做服务端出向/上游 TLS 校验，依赖它。
- `crates/aria-agent-cloud/Cargo.toml`：显式钉住 `webpki-roots = "1.0.10"`，防止锁漂移回受影响版本。
- 核验：`cargo tree -i webpki-roots` 须 >= 1.0.10（二进制信任库编译进产物，不走系统 bundle；本仓 `--locked` 构建已锁定）。

### 2. JS SDK（bun）
- `sdk/js/package.json`：`engines` 新增 `bun >=1.4.4`（Bun 1.4.4 修复其 TLS 信任库；SDK 用 `bun test` 构建/测试）。

### 3. Python SDK / 容器
- `sdk/python/pyproject.toml`：`dependencies = []`，不自带信任库，依赖主机/容器 `ca-certificates`。
- `Dockerfile`：构建(`rust:1-bookworm`)与运行时(`debian:bookworm-slim`)均 `apt-get install ca-certificates`（已具备），重建镜像即更新系统信任库。
- 新增 `scripts/check-emudhra.sh`：离线扫描容器系统 bundle（以 emSign 根为信任锚），规避前向/拦截代理造成的在线探测假阳性。

### 4. 范围说明
- 本仓库**无 Go**；Node 仅作 JS SDK 运行/构建运行时（engines `node >=18`），其 TLS 信任库随 Node 发布更新（建议 >= 22.23.4 / 24.22.1 / 26.12.1）。
- 站点证书（部署用 Let's Encrypt）不受影响，无需重签。

## Milestone 17 — test system upgrade (100% line coverage)

- [x] `aria-agent-cloud` split into a library crate root (`src/lib.rs`) plus a thin
      `src/main.rs`; `AppError`, `Principal`/auth, `AppState`, `api::keys`, `util`
      moved into modules so the handlers are unit-testable without a process.
- [x] Data access behind traits: `AgentStore` / `SessionStore` / `ApiKeyStore`
      (`store/mod.rs`), with `store/memory.rs` (tenant isolation, `created_at DESC`,
      `limit`, turn/item `seq`) used by tests and `store/pg.rs` keeping the
      production SQL verbatim (excluded from the gate: needs a live database).
- [x] API tests via `tower::ServiceExt::oneshot`: agents CRUD (400/404/500),
      sessions/turns/items, memory + relations + graph, api-keys, stubs, and the
      SSE stream (`agent.turn.created` → `item.*` → `output_text.*` →
      `agent.turn.completed`, plus `agent.turn.failed`).
- [x] `aria-agent-sandbox`: `DockerSandbox::with_client` injection point plus a
      hand-rolled Engine API stub over TCP (raw-stream `exec` upgrade) covering
      spawn/exec/destroy success and every failure branch.
- [x] `aria-agent-memo`: storage edge cases (corrupt blobs, unusable paths, unknown
      memo types, poisoned locks, dropped tables, invalid relation kinds); the CLI
      interop tests moved behind the `cli-interop` feature.
- [x] `aria-agent-core`: shared `model_err`/`db_err`/`prefixed` mappers, composite
      merge/dedupe/ordering, graph degradation, `run_stream` dedupe, env guards.
- [x] `aria-agent-browser`: every `browser_*` command, argument validation, image
      override, `Unsupported`/`Engine` degradation, crashing runner.
- [x] `ariacompute-agent`: `CloudContextStore` over a loopback stub (all REST verbs,
      4xx/5xx, URL encoding), `store_for` backend combinations, `map_event`.
- [x] Gate: `scripts/coverage.sh` (`just cov-check`) requires 100% of lcov line
      records outside the documented exclusions; `.github/workflows/ci.yml` runs
      fmt + clippy + `cargo test` + the gate.
- [x] Docs: `docs/coverage.md` (exclusions, doubles, conventions).

Acceptance:

```bash
cargo test --workspace   # green
just cov-check           # 100% lines outside docs/coverage.md exclusions
```

## Milestone 8 — remove codex
- [x] Deleted the `codex/` submodule, `.gitmodules`, the codex git dependencies,
      the exact `rama-*` pins, the `[patch.crates-io]` section and the Windows
      `winapi` patch entry.
- [x] Deleted `CodexSandbox` / `SandboxProvider::Codex`; `codex` now parses to
      `None` and the cloud fails with a configuration error (ADR-0018).
- [x] Deleted ADR-0001 / ADR-0005; rewrote ADR-0003 / ADR-0012 and every doc that
      referenced the submodule.

## Milestone 9 — plugin kernel & seams
- [x] New `aria-agent-plugin`: `SeamKey`, `SeamRole`, `PluginManifest`,
      `Registry::freeze` with duplicate/`core`-uniqueness/topo-sort/cycle checks and
      `PluginError` (ADR-0014).
- [x] `aria-agent-core` split into `agent_loop`, `prompt`, `token_meter`,
      `seams/*` and `session/*`; seams are resolved once in `Agent::with_handlers`.

## Milestone 10 — session event stream
- [x] `SessionEvent` + `SessionRecorder` (batch flush, retry-on-failure) and
      `SessionProjection::replay` (turns/items/title/todos/plan/usage/compactions).
- [x] JSONL persistence, cursor query (`from_seq`), token meter, pressure
      compaction, overflow compaction with a single retry, deterministic session
      title plus an optional async provider, ordered system-prompt assembly
      (ADR-0015).

## Milestone 11 — tool pipeline & approvals
- [x] `ToolExecutionPipeline`: resolve → preset → approval → dispatch →
      post-process (truncate + spill); `AgentEvent::Approval` / `Usage`
      (ADR-0016).
- [x] `SandboxPolicy::workspace` single root; `read_only` / `workspace_write` /
      `danger_full_access` presets; fail-closed tests for missing broker / web
      provider / fs root.

## Milestone 12 — built-in tool families
- [x] `aria-agent-tools`: fs / shell / jobs / terminal (`pty`) / skills / web
      (`web`) / ask_user / plan / todo / memo, each with normal + abnormal tests.
- [x] `HttpWeb` provider (SSRF gate, redirect cap, body cap, content-type
      whitelist); memo tools moved out of `aria-agent-core`.

## Milestone 13 — orchestration
- [x] `aria-agent-orchestrator`: in-process sub-agents (namespaced child session,
      concurrency cap, cached `wait`), agent teams (validated DAG, gated claims),
      scheduled jobs (single-flight ticker, advance-before-run) (ADR-0017).

## Milestone 14 — cloud & SDK surface
- [x] `migrations/0006..0008` (`scheduled_jobs`, `agent_teams`, `session_titles`),
      Postgres `TeamStore` / `ScheduleSeam`, `/v1/agent-teams` and
      `/v1/scheduled-jobs` routes, orchestrator tools wired per principal.
- [x] Session titles: the cloud installs a `SessionPersistence` projection that
      writes `SessionEvent::TitleSet` into `session_titles` (deterministic title
      once, provider override may replace it) and exposes
      `GET /v1/agents/sessions/{id}/title`.
- [x] Cloud sub-agents: `InProcessSubagents` + `CloudSubagentsFactory` rebuild the
      agent for each child with `allow_subagents = false` (depth one), so
      `subagent_run` / `subagent_list` are real tools in the cloud.
- [x] Loop parity for titles: `run` / `run_stream` now record `turn_started`,
      `title_set` (flushed before the model call) and a terminal event, so a
      delegated child (whose provider calls `run`) gets a `session_titles` row.
      Child ids are parsed with `seams::child_session` / `parent_session`, and
      `GET /v1/agents/sessions/{child}/title` is authorised through the parent.
- [x] `SdkAgentConfig` gains `permission_preset` / `approval_mode`; bindings
      regenerated via `just ffi`; JS/Python SDKs decode `agent.turn.approval.*`
      and `agent.turn.usage` and expose `result.usage`.

## Milestone 15 — docs & verification
- [x] ADR-0014..0018, rewritten `docs/architecture.md`, `docs/coverage.md`,
      README/README_cn, AGENTS.md rules 4/14/15.
- [x] `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`,
      `cargo test --workspace`, `just ffi`, `just sdk-js-test`, `just sdk-py-test`.

## Milestone 16 — DeepSeek Harness runtime (ADR-0019)
- [x] `agent_loop` seam: `LoopContext` + `AgentLoop` (`run` / `run_once`); old loop
      moved to `NativeLoop` with identical events/session writes.
- [x] `AgentConfig::harness` (`native` | `dsh`), `default_agent_loop` and
      `Agent::with_components`; unknown values and an uninjected `dsh` fail closed.
- [x] `aria-agent-dsh`: stdio JSON-RPC transport (frame cap, stderr drain, kill),
      client (timeouts, pending table, server requests), per-session pool
      (capacity + idle reap), approval/question bridge, event mapping.
- [x] Cloud wiring: `AGENT_HARNESS` (default `dsh`), `DSH_BIN`,
      `DSH_HOME_ROOT`, `DSH_POOL_MAX`, `DSH_IDLE_TTL_SEC`; one pool per principal.
- [x] Native SDK: `ARIA_AGENT_HARNESS` opt-in (embedded default stays `native`).
- [x] Seam contract alignment: `service` role, dsh aliases, precedence order,
      `dispose`.
- [x] Tests: fake transport / scripted runtime / real scripted child process;
      `just dsh-smoke`; env-gated live turn `just dsh-e2e`.
- [x] Docs: ADR-0019, AGENTS.md rule 16, architecture "Harness boundary",
      requirements §10, release notes.

## Milestone 17 — Rust/Swift/Kotlin SDKs become cloud clients (ADR-0020)
- [x] `ariacompute-agent` rewritten as a thin HTTP client: blocking `reqwest`
      transport, SSE line reader, typed `AriaError` (`config`/`auth`/`network`/
      `api`/`unknown`).
- [x] JS/Python vocabulary on the FFI: `AriaAgent` (constructor, `createSession`,
      `session`, `run`, `runStreamed`), `Session` (`id`, `memoryBackend`,
      `memorize`, `recall`, `itemsJson`, `turnsJson`, `titleJson`,
      `historyJson`), `StreamListener`, `RunResult`, `TokenUsage`.
- [x] Native path removed: no `aria-agent-core` / sandbox / tools / model client
      / embedded context store; `cloud_context.rs`, `create_agent_for_tenant*`,
      `sandbox_provider` / `permission_preset` / `approval_mode` deleted.
- [x] Memory keeps `cloud` (session memory REST), `local` (aria memo CLI) and
      `both` (write both, read merged, cloud wins).
- [x] Bindings regenerated (`just ffi`); README/architecture/requirements/
      AGENTS.md updated; hermetic stub-server tests cover request shape, SSE
      decoding, error mapping and the CLI seam.

## Milestone 18 — real-socket E2E coverage for governance + CRUD
- [x] `tests/e2e_http.rs`: governance egress-policy scenario over the socket —
      read/write scope gates (403), revision-guarded `PUT`/`PATCH` (200/409),
      invalid mode/host (400) and cross-principal `deny_all` isolation.
- [x] Governance audit/usage/status scenario: tenant-filtered audit list,
      `kind` filter + bad kind, NDJSON export, state-split usage summary and
      the boot-wired status flags; all driven through `build_router`.
- [x] Governance credentials/checkpoints scenario: missing-store/backend fail
      loudly (400), checkpoint seed → list, unknown restore → 404, known
      restore without a sandbox → 400.
- [x] Agent/session CRUD scenario: create → update → list → update → delete →
      404 on both resource types (was covered only by oneshot/curl smoke).
- [x] `just e2e` now runs 9 real-socket scenarios (was 5); fmt, clippy
      `-D warnings`, `cargo test --workspace` and `just cov-check` stay green.

## Milestone 19 — Recursive self-improvement (ADR-0026)
- [x] Core kernel, no I/O: `session::insight` folds the existing record stream
      into `EfficiencyReport` / `TurnInsight` / `Pathology` with per-finding
      evidence (`seq`, tool, turn), `max_records` / `max_turns` caps and an
      explicit `truncated` flag; `RsiConfig` (default `enabled = false`) and
      `session::ChainPersistence` (authoritative first sink, best-effort rest).
- [x] Event contract stayed additive: `ItemDone` gained
      `latency_ms: Option<u64>` with `#[serde(default)]`; **no** new
      `SessionEvent` variant, so old JSONL and the frozen SSE envelope are
      unaffected.
- [x] New `aria-agent-rsi` crate (`publish = false`, depends on
      `aria-agent-core` only): `RsiLedger` (per `(principal, session, seq)`
      dedupe + window queries), `DeterministicReflection` (pathology → candidate
      rule table with scope + parameter list, zero model calls),
      replay `validate` (score vs baseline, discard below `min_score_gain`),
      `PlaybookEngine` (revision-guarded `current` / `propose` / `promote` /
      `list_versions`), `FilesystemSkillSink`, `RsiSweep` (single-flight).
- [x] New seams `rsi` and `playbook` (`PlaybookStore`, `ReflectionEngine`,
      `RsiScope`, `Adaptation`, `ValidationOutcome`) registered in
      `aria-agent-plugin`'s `SeamKey::ALL`; an unregistered key still fails at
      boot with `UnknownSeam`.
- [x] Cloud storage: migration `0009_rsi.sql` (`rsi_reports`, `rsi_proposals`,
      `rsi_playbooks`; unique `(principal_id, domain, key, revision)`),
      `store::RsiStore` / `PlaybookStore` ports, `PgStore` implementation with
      `principal_id` filtering and single-transaction batches, and an in-memory
      provider so the same semantics are covered offline.
- [x] HTTP surface `api/rsi.rs`: `GET /v1/rsi/status`, `POST /v1/rsi/sweep`,
      `GET /v1/rsi/reports[/{id}]`, `GET /v1/rsi/proposals`,
      `POST /v1/rsi/proposals/{id}/promote|discard`,
      `GET /v1/rsi/playbooks/{key}[/versions]`,
      `POST /v1/rsi/playbooks/{key}/adapt`. Views never expose `principal_id`;
      errors reuse `AppError`.
- [x] Gate semantics: RSI off → `503`; promote with stale `expected_revision`
      → `409` (never last-writer-wins, and the pending row survives so the
      approver can retry); unadapted cross-tenant adapt → `422`;
      non-admin adaptation → `403`; unknown proposal/playbook → `404`.
      With `AGENT_RSI_AUTO_PROMOTE` off the human gate is the only path.
- [x] Real re-run A/B is a separate switch (`AGENT_RSI_REAL_RERUN`); this build
      ships no re-run probe, so enabling it is a **startup error** instead of a
      pretend second gate.
- [x] Startup wiring: `RsiState` in `AppState`, ledger appended through the
      session-persistence chain, `AGENT_RSI_SWEEP_THRESHOLD` / `_SECS`
      (default `0` = external cron drives `POST /v1/rsi/sweep`) and
      `AGENT_RSI_SKILLS_DIR` (default `.aria/skills`).
- [x] dsh reuse: `DshConfig::for_session` links `<workspace>/.aria/skills` to the
      shared catalog (symlink on Unix, copy elsewhere), skipped when nothing has
      been distilled and best-effort on failure — a missing skill degrades
      capability, it never fails a turn.
- [x] Tests: inline normal + failure cases in every new core/rsi file, memory
      provider coverage for the store ports, and 8 HTTP scenarios over the real
      router (`rsi_tests.rs`) covering disabled/409/422/403 and the happy path.
      `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace` and
      `just cov-check` stay green.
- [x] Docs: ADR-0026, `requirements.md §11`, this milestone, architecture
      layering + storage boundary, and an AGENTS.md rule (off by default,
      additive events, explicit gate, no hot swap, adaptation before reuse).

## Open follow-ups
* Surface `AgentEvent` through the native SDKs (Swift / Kotlin) so in-process
  runs render the same timeline as the cloud stream.
* Publish the six browser images and wire `runner.mjs` into the release pipeline
  (currently image-build artifacts only; runtime orchestration is tested without
  a daemon).
* Downstream migrations (not in this repo): `docs/followups/playground-migration.md`,
  `docs/followups/serve-copy-migration.md`, `docs/followups/cockpit-client-migration.md`.
