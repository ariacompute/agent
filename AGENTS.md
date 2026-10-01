# AGENTS.md — contribution & architecture conventions

This repo is a Rust workspace that wraps the OpenAI **codex** harness and ships a
cloud API (OpenAI **beta Agents** compatible) plus native (Swift/Kotlin) and
language (JS/Python) SDKs.

## Workspace layout

* `crates/aria-agent-core`, `aria-agent-sandbox`, `aria-agent-memo` (aria memo),
  `ariacompute-agent`, `aria-agent-cloud`, `aria-agent-ffigen`,
  `aria-agent-browser` (browser sandbox: six-image `BrowserCatalog` +
  `BrowserSandbox` + browser tool family)
* `bindings/swift` (`Package.swift` SwiftPM + `AriaComputeAgent.podspec` CocoaPods),
  `bindings/kotlin` (Android `build.gradle.kts` with vanniktech Maven publish).
  Generated Swift/Kotlin *sources* are committed and must not be edited by hand —
  regenerate via `just ffi`; the podspec and Gradle publishing config are hand-maintained.
* `sdk/js` — npm package `@ariacompute/agent` (mirrors `@openai/agents`).
* `sdk/python` — pip package `ariacompute-agent` (mirrors `openai-agents`).
* `codex/` is a **git submodule** (do not add it to the Cargo workspace).
* `crates/aria-agent-cloud/src/event_envelope.rs` is the frozen SSE envelope:
  `AgentEvent` → OpenAI **beta Agents** streaming frames (see rule 9).

## Rules

1. **Context storage is tiered, and SDKs can switch backends.**
   * **Cloud**: conversational / long-term context lives in **Postgres +
     pgvector** (`context_fragments`), sharded by `principal_id`. pgvector is a
     hard requirement — boot fails if the `vector` extension is missing
     (`docs/adr/0010-context-storage-pgvector.md`).
   * **Local**: `aria-agent-memo` is the **aria memo** store (SQLite, the same
     `memories` schema as the `aria-memo` CLI). The sled implementation was
     deleted — there is no second on-device format.
   * **Every SDK selects `cloud` / `local` / `both`** via constructor config
     with per-call overrides; `both` writes twice and reads merged + deduped
     (`docs/adr/0011-memory-backend-switch.md`).
   * The shared contract is `aria-agent-core::context` (`ContextStore`,
     `ContextFragment`, `FragmentKind`, `RecallQuery`, `LocalEmbedder`,
     `MemoryBackend`, `CompositeContextStore`, ranking helpers). Never bypass it
     with ad-hoc queries.
2. **FFI is stable.** Only change `ariacompute-agent`'s exported surface
   deliberately. After any change run `just ffi` and commit the regenerated
   bindings. See `docs/adr/0002-ffi-boundary.md`. The public SDK ergonomics
   (session-less happy path) are specified in `docs/adr/0008-sdk-interface.md`.
3. **Sandbox is pluggable.** Add providers by implementing the `Sandbox` trait
   and registering them in `from_provider`. Docker is the default. The cloud
   selects the **codex** backend (`sandbox_provider = "codex"`), resolved by this
   runtime to `codex_sandbox::CodexSandbox` (ADR-0005); Docker/Kata/Cube remain
   available through `from_provider`. The **browser** provider
   (`sandbox_provider = "browser"`) is backed by `aria-agent-browser`'s
   `BrowserSandbox`, which reuses the same Docker/Kata container path and
   `SandboxResourceLimits`. See `docs/adr/0003-sandbox-providers.md`,
   `docs/adr/0005-codex-integration.md`, and `docs/adr/0012-browser-sandbox.md`.
4. **Submodule discipline.** Keep `codex` out of the workspace member list;
   reference codex crate shapes by design. See `docs/adr/0001-submodule-strategy.md`.
5. **Secrets/logging.** Use `tracing`. Never log the OpenAI key, raw user content,
   or embedding vectors.
6. **Tests.** `cargo test --workspace` must pass; JS SDK via `just sdk-js-test`
   (`bun test`), Python SDK via `just sdk-py-test`. Cross-crate behavior belongs
   in `tests/`. DB-backed tests are gated on `DATABASE_URL` (skip when unset,
   never fabricate results). New features need normal + abnormal path coverage.
7. **Cloud auth + multi-tenancy.** `aria-agent-cloud` resolves each caller to a
   `Principal` (tenant) from `Authorization: Bearer <key>` / `ApiKey <key>`,
   looked up (sha256) in Postgres `api_keys` (`AGENT_CLOUD_API_KEY` is the
   bootstrap admin key; open mode when unset). Resolved keys are cached with a
   revocation TTL (`API_KEY_CACHE_TTL_SEC`, default 60); `DELETE /v1/api-keys/:id`
   purges the cache. Every table (`agents`, `context_fragments`,
   `agent_sessions`, `session_turns`, `session_items`) carries `principal_id`
   and every query filters on it.
8. **Cloud API = OpenAI beta Agents.** Implemented: `POST|GET /v1/agents`,
   `GET|POST|DELETE /v1/agents/{id}`, `POST|GET /v1/agents/sessions`,
   `GET|POST|DELETE /v1/agents/sessions/{id}`,
   `POST /v1/agents/sessions/{id}/events`,
   `POST /v1/agents/sessions/{id}/events/stream`, read-only `…/items`,
   `…/turns`, `…/subagents`. `vaults`, `agents/environments` (+files/templates)
   and `…/artifacts` return **501**. Requests may carry `OpenAI-Beta: agents=v1`.
   See `docs/adr/0009-openai-beta-agents-api.md`.
9. **Streaming contract is the beta Agents envelope and is frozen.**
   `event_envelope.rs` is the single translation point from `AgentEvent` to
   `agent.*` frames: `Step` → `agent.turn.in_progress` (+ `phase` / `label`),
   `ToolCall` → `agent.turn.item.added` + `agent.turn.item.done` (with `result`),
   `Token` → `agent.turn.item.added` (message) + `agent.turn.output_text.delta`,
   `Done` → `agent.turn.output_text.done` + `agent.turn.item.done` +
   `agent.turn.completed`, errors → `agent.turn.failed`. Every frame carries
   `event_id`, `session_id`, `turn_id` and a monotonic `sequence_number`;
   `agent.turn.completed` (or `agent.turn.failed`) is the **only** terminal event
   and there is **no** trailing `data: [DONE]` sentinel. Changing this contract
   is breaking for every SDK and downstream — update `event_envelope.rs`, its
   tests, both SDKs, the READMEs and the binding examples together.
10. **Default agent seed.** `ensure_schema` seeds `id: agent-demo` /
    `name: Agent Demo` (admin-owned) and renames a legacy `playground-demo` row
    in place. Both statements are idempotent.
11. **Downstream migrations are follow-ups, not in-repo edits.** The playground,
    serve and cockpit adaptations are tracked in `docs/followups/*.md`; do not
    edit those repositories as part of an agent-repo change.
12. **Browser sandbox (sandbox-embedded browser).** The `aria-agent-browser`
    crate provides a pluggable `BrowserEngine` and a `BrowserSandbox` that
    reuses the existing Docker/Kata container path and `SandboxResourceLimits`.
    A `BrowserCatalog` registers six browser images — `servo`, `obscura`,
    `chromium`, `gosub`, `camoufox`, `lightpanda` — each a separate sandbox
    container image (Playwright-family Chromium/Camoufox/Lightpanda share a base
    image + per-engine runner; Obscura is a Chromium stealth layer; Servo/Gosub
    are minimal native-engine images). Images are overridable per kind via
    `ARIACOMPUTE_BROWSER_IMAGE_<KIND>`. The agent can **dynamically
    select/switch** the active browser image at runtime (`sandbox_provider =
    "browser"`, default kind `chromium`) and via the `browser_use` tool, which
    tears down the current container and re-pulls the chosen image, rolling back
    safely on failure (no panic / no dropped connection). Browser capabilities
    (navigate / extract / click / fill / screenshot / evaluate / solve_captcha)
    are exposed as a browser tool family routed through a `ToolHandler` before
    the shell fallback; native engines (servo, gosub) gracefully degrade
    unsupported ops to `BrowserError::Unsupported`. See
    `docs/adr/0012-browser-sandbox.md`.
13. **Memo is fully wired into the run loop.** The `aria-agent-memo` store is not
    just a recall/memorize backend — the agent (a) supplements vector `recall`
    with bounded graph `expand` (`RelationKind::Temporal` edges between each
    turn's user↔assistant fragments), (b) auto-`compact`s a session once its
    fragment count exceeds `AgentConfig::memory_compact_threshold`, and (c) exposes
    a long-term-memory tool family `memo_store` / `memo_get` / `memo_search` /
    `memo_related` via `MemoToolHandler` (a `ToolHandler` routed before the shell
    fallback). The graph + auto-compact are gated by `AgentConfig::memory_graph`
    (default **off** in `aria-agent-core` so its unit tests stay hermetic; the SDK
    enables both, `memory_graph = true`, `threshold = 32`). `aria-agent-core` must
    **not** depend on `aria-agent-memo` — it only depends on the `ContextStore`
    trait, so local (SQLite) and cloud (pgvector) backends are interchangeable.
    See `docs/adr/0013-agent-memo-integration.md`.

## SDK（js / python）

端侧/服务端通用的 agent SDK，API 形状对齐 OpenAI Agents SDK（`Agent` / `run` /
`runStreamed` / `tool` / `Session` / `result.finalOutput` / `result.history`）。
`bindings/swift`、`bindings/kotlin` 由 `ariacompute-agent` 的 UniFFI 表面生成，
语义与 js/python 一致（见规则 2）。

### 分层

`transport`（HTTP/SSE，可注入 `fetch`/urllib） → `session`（会话 + 只读
items/turns + 记忆 backend） → `runner`（`run`/`runStreamed` + `maxTurns` + 事件解码）
→ `agent`/`tool`（配置与工具） → `memory`（cloud/local/both，保持）。所有模块
零网络/零 LLM 可单测（js 用 `bun test` 注入 `fetch`，python 用 `unittest`
mock `transport._request`）。

### 公开 API 契约

- `Agent(config)`：`name` 必填且非空（空白报错）；`tools` 名称必须唯一（重复抛
  `duplicate tool name`）；`toolSchemas()` 不透传 `execute`，默认 `parameters`。
- `run(agent, input, { session?, client?, maxTurns? })`：单轮阻塞；`maxTurns`
  须为正整数（否则 `config` 型错误）；返回 `RunResult`（finalOutput/history/
  lastAgent/sessionId/turnId）。
- `runStreamed(agent, input, { session?, client?, maxTurns? })`：SSE 流；按
  `agent.turn.created` 计数，超过 `maxTurns` 早退并抛 `config` 型错误；`events`
  为已解码 `agent.*` 帧，`completed` 在终端帧到达后解析。
- `Session`：`memorize`/`recall` 走 cloud/local/both；`getItems()`/`getTurns()`
  是只读端点（GET `…/items`、`…/turns`，不改动上下文，对应规则 8）。
- `memory`：`cloud` = agent-cloud REST；`local` = aria memo（js 经
  `aria-memo` CLI、python 直连 SQLite `memories` 表，`memo.db` 与 CLI 同格式）；
  `both` = 双侧写入、读取合并（cloud 优先、local 兜底），单侧失败可容忍。

### 异常

传输失败统一归一为类型化错误：`AriaError`，`kind ∈ {auth, network, api,
config, unknown}`，`api`/`auth` 带 HTTP `status`。401/403 → `auth`，其他非 2xx →
`api`，连接失败 → `network`，`maxTurns`/缺 `fetch` 等本地误用 → `config`。
`memory` 的 `both`：单侧失败容忍，双侧同时失败才抛错。解码未知 `agent.*` 帧
透传、不中断流；`data: [DONE]` 与保活/残缺帧被忽略（终端帧为
`agent.turn.completed`/`agent.turn.failed`，无 `[DONE]` 哨兵）。

### 验收

- `just sdk-js-test`（51 用例）与 `just sdk-py-test`（28 用例）全绿：覆盖正常 +
  异常（类型化错误 kind、未知 backend、`getItems`/`getTurns`、空/数组输入、
  `maxTurns` 上限、单侧 backend 失败、解码回退）。
- 冻结约束：不改 `event_envelope.rs`（SSE 契约），不改 `memory` 三层结构，不引入
  memo 记忆内部实现。
- 日志沿用 `tracing`，不打印 OpenAI key / 原始用户输入 / 嵌入向量。

## Common commands

* `just build` — build all crates
* `just test` — `cargo test --workspace`
* `just sdk-js-test` / `just sdk-py-test` — SDK tests
* `just ffi` — regenerate Swift/Kotlin bindings
* `just cloud` — run the cloud service (needs Postgres **with pgvector**;
  `aria-agent serve` is the default subcommand). `aria-agent serve --port <n>`
  (default `CLOUD_PORT` or 3000).
* `aria-agent setup` / `aria-agent upgrade [version] [--url <url>]` — CLI
  self-management (`~/.ariacompute/agent-cli.yml`).
* `just migrate` — `sqlx database create` + `sqlx migrate run`
* `just fmt` / `just lint` — formatting and clippy
* Release & publish: `.github/workflows/release.yml` (binary + cdylib assets),
  `publish-cargo.yml` (crates.io: sandbox → core → memo → ariacompute-agent),
  `publish-maven.yml` (Maven Central), `publish-npm.yml`, `publish-pypi.yml`;
  Swift via CocoaPods `bindings/swift/AriaComputeAgent.podspec`.

Windows build (release matrix `windows-x86_64`): `aria-agent-cloud` pulls
`codex-utils-pty`, whose conpty code mixes `winapi::ctypes::c_void` with
`std::ffi::c_void`. Those are the same type **only** with winapi's `std`
feature, so the crate declares a Windows-only `winapi = { features = ["std"] }`
dependency — do not remove it, or the Windows build fails with three E0308s.
