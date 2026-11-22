# AGENTS.md — contribution & architecture conventions

This repo is a Rust workspace: a capability-seam agent runtime (`aria-agent-protocol`
seams + plugin registry) that ships a cloud API (OpenAI **beta Agents** compatible),
native (Swift/Kotlin) and language (JS/Python) SDKs. Every capability is a seam with
a default provider, and boot fails closed when the wiring is inconsistent. The
default **turn runtime** is the external **DeepSeek Harness** (`dsh`, driven as a
child process over JSON-RPC — no vendored TypeScript); the in-process Rust loop is
kept as an explicit `native` fallback (ADR-0019).

## Workspace layout

* `crates/aria-agent-core` (loop, seams, session event stream, prompt assembly,
  token meter), `aria-agent-dsh` (the `agent_loop` seam's dsh backend: JSON-RPC
  stdio transport, per-session runtime pool, approval bridge, event mapping),
  `aria-agent-plugin` (manifest + registry + freeze),
  `aria-agent-tools` (fs/shell/jobs/terminal/skills/web/ask/plan/todo/memo families),
  `aria-agent-orchestrator` (sub-agents, teams, scheduled jobs), `aria-agent-sandbox`,
  `aria-agent-memo` (aria memo), `ariacompute-agent` (UniFFI cloud API client
  for Swift/Kotlin — no embedded runtime), `aria-agent-cloud`,
  `aria-agent-ffigen`, `aria-agent-browser` (browser sandbox: six-image
  `BrowserCatalog` + `BrowserSandbox` + browser tool family)
* `bindings/swift` (`Package.swift` SwiftPM + `AriaComputeAgent.podspec` CocoaPods),
  `bindings/kotlin` (Android `build.gradle.kts` with vanniktech Maven publish).
  Generated Swift/Kotlin *sources* are committed and must not be edited by hand —
  regenerate via `just ffi`; the podspec and Gradle publishing config are hand-maintained.
* `sdk/js` — npm package `@ariacompute/agent` (mirrors `@openai/agents`).
* `sdk/python` — pip package `ariacompute-agent` (mirrors `openai-agents`).
* **Trust stores (eMudhra/emSign CA key compromise, 2026-10)**: Rust outbound TLS
  goes through `webpki-roots` (pinned `>=1.0.10` in `aria-agent-cloud` — removes the
  compromised emSign roots); the JS SDK is built/tested with **bun >=1.4.4**; the
  Python SDK bundles no trust store and relies on the host/container `ca-certificates`.
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
2. **Every SDK is a cloud API client — the FFI is stable but not embedded.**
   `ariacompute-agent` is a thin HTTP client (`AriaAgent` / `Session` / `run` /
   `runStreamed` / `memorize` / `recall`, JS/Python vocabulary); it must never
   link `aria-agent-core`, a sandbox, tools or a model client again — device
   binaries carry no runtime and no provider key. Only change the exported
   surface deliberately, then run `just ffi` and commit the regenerated bindings.
   See `docs/adr/0002-ffi-boundary.md`, `docs/adr/0008-sdk-interface.md` and
   `docs/adr/0020-sdk-cloud-client.md` (the native path was removed).
3. **Sandbox is pluggable.** Add providers by implementing the `Sandbox` trait
   and registering them in `from_provider`. Docker is the default. The cloud
   default is **docker** (ADR-0003/ADR-0018); Kata/Cube remain available through
   `from_provider`, and an unknown provider name (including the removed `codex`
   value) is a hard configuration error, never a silent fallback. The **browser** provider
   (`sandbox_provider = "browser"`) is backed by `aria-agent-browser`'s
   `BrowserSandbox`, which reuses the same Docker/Kata container path and
   `SandboxResourceLimits`. See `docs/adr/0003-sandbox-providers.md`,
   `docs/adr/0003-sandbox-providers.md` and `docs/adr/0012-browser-sandbox.md`.
   Egress enforcement is structural, not advice: with `egress_network` set,
   `Agent::with_*` wraps the injected backend in `NetworkingSandbox`, which fills
   `network`/`labels`/`env` only where the caller left them empty. Backends that
   cannot honour them (cube) fail closed; `proxy_env` lives in
   `aria-agent-sandbox` and is the single source of the proxy variable set
   (ADR-0024).
4. **Capability seams + plugin registry.** Every capability is a seam
   (`llm`, `sandbox`, `fs`, `shell`, `jobs`, `storage`, `sessionPersistence`,
   `sessionQuery`, `approvals`, `skills`, `web`, `userQuestions`, `subagents`,
   `schedule`, `egress`, `credentials`, `audit`, `metering`, `agent_loop`).
   Roles follow dsh's four kinds (`core` / `seam` /
   `bundle` / `service`); keys accept dsh's spellings as aliases, resolution order
   is registration order (precedence), and `service` keys reject providers.
   Providers register in `aria-agent-plugin`; assembly topo-sorts `requires`,
   rejects duplicates/cycles and enforces one provider per `core` key at boot.
   Resolve seams **once** in `Agent::with_components` — never per tool call.
   Tools run through `ToolExecutionPipeline`: resolve → permission preset →
   approval → dispatch → post-process; **no approval provider means denied**, and
   the reason is fed back to the model. See ADR-0014/ADR-0016/ADR-0019.
5. **Secrets/logging.** Use `tracing`. Never log the OpenAI key, raw user content,
   or embedding vectors. Credentials travel as the literal `${credential}`
   placeholder from the agent side; the plaintext only exists inside the egress
   boundary (ADR-0025). Audit payloads are redacted by key whitelist before they
   are written (ADR-0021).
5b. **Governance defaults are closed, and there is exactly one source of them.**
   Egress is deny-by-default; `allowlist` with no hosts fails boot. `AgentConfig`
   (`egress_mode`, `egress_allow`, `egress_deny`, `egress_enforcement`,
   `audit_sink`, `metering`, `compression`, `idle_timeout_secs`, ...) is the only
   place defaults are defined — env vars (`AGENT_EGRESS_*`, ...) may override a
   value but never introduce a different default, and the resolved values are
   logged at startup. Policy mutations are revision-guarded: a stale write gets
   `409 Conflict`, never a last-writer-wins overwrite. `SandboxLifecycle` and task
   state are orthogonal — never report one as the other. Credential rotation has
   no cache window. Idle detection uses five signals (HTTP, exec, processes,
   connections, jobs), never inbound HTTP alone. See ADR-0021..0025.
5c. **Egress enforcement modes are not interchangeable.** `proxy` sets
   `HTTP_PROXY`; `network` removes the default route (Docker `internal` network +
   `aria-agent-egress-proxy` sidecar, ADR-0024) and **fails boot** if the network,
   the image or the running proxy is missing — never fall back to `proxy`. The
   sidecar holds no secrets or policy: it calls `/internal/egress/*` with
   `X-Aria-Proxy-Token`, and those routes exist only when enforcement is
   `network`. Sandbox identity comes from the `aria.tenant_id` / `aria.session_id`
   labels written at creation; an unknown peer is refused, and a principal that
   does not own the calling peer IP is rejected. Publish the sidecar port nowhere
   and do not route `/internal/egress/*` through nginx.
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
   `…/turns`, `…/subagents`, `…/title`, plus `/v1/agent-teams` and
   `/v1/scheduled-jobs`. `vaults`, `agents/environments` (+files/templates)
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
    14. **The session event stream is the source of truth.** `SessionEvent`
     (`turn_started` / `step` / `item_added` / `item_done` / `text_delta` /
     `text_done` / `approval` / `usage` / `compacted` / `title_set` /
     `turn_completed` / `turn_failed`) is append-only; `SessionProjection::replay`
     folds it into turns/items/title/usage, and the Postgres `session_turns` /
     `session_items` tables are a **projection**, never a second source. Local
     persistence is one JSONL file per session, flushed once per turn. **Every**
    entry point records the same vocabulary — `run_event_stream`, `run` and
    `run_stream` all emit `turn_started` + `title_set` (flushed before the model
    is called, so a slow or failing provider cannot lose the title) and a
    terminal event. Usage comes
     from the provider when reported and is only estimated locally as a fallback;
     an overflow report compacts once and retries once. See ADR-0015.
    15. **Orchestration is tenant-scoped.** Sub-agents get their own session id
     (`<parent>::sub::<id>`) and never share the parent's event stream; a team is a
     validated task DAG (unknown ids, duplicates and cycles are refused *before*
     any claim); scheduled jobs fire through a single-flight ticker that advances
     `next_run_ms` **before** running. `agent_teams`, `scheduled_jobs` and
     `session_titles` all carry `principal_id` and every query filters on it. See
     ADR-0017.

    16. **DeepSeek Harness (dsh) is the default turn runtime.** `AgentConfig::harness`
     selects the `agent_loop` seam: `native` (library default) or `dsh` (cloud default,
     injected via `Agent::with_components` with `aria-agent-dsh`'s `DshAgentLoop`).
     Unknown values, and a `dsh` config without an injected loop, are hard errors —
     never a silent fallback. dsh is an **external child process**
     (`dsh --profile sdk`, newline-delimited JSON-RPC over stdio), not a vendored
     TypeScript harness and not a third-party gateway plugin. Under `harness=dsh`,
     **dsh executes tools in its own sandbox**: aria's `Sandbox`,
     `ToolExecutionPipeline` and registered handlers do not run for that session;
     aria still owns recall, memory writes, session events/titles, approval and
     question bridging, and the frozen wire envelope. Approvals and questions use
     the installed seams and fail closed. Each session gets its own `DSH_HOME` and
     workspace (`DshConfig::for_session`), the child environment is an allow-list,
     and `~/.dsh` is never discovered. By default (`DSH_THINKING_FILTER=enable`,
     set in `.env`) dsh reaches the gateway through the loopback
     `ThinkingFilterProxy`, which strips Anthropic `redacted_thinking` blocks in
     both directions (the bundled adapter cannot replay them) while keeping
     `thinking`; loopback is forced into the child's `NO_PROXY`. On a dsh upgrade
     only `aria-agent-dsh/src/event_map.rs` should need changes. See ADR-0019.

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

- `just sdk-js-test` 与 `just sdk-py-test` 全绿（CI `sdk` job 同样执行
  `bun test` 与 `python3 -m unittest discover -s tests`；用例数不在此硬编码）：
  覆盖正常 + 异常（类型化错误 kind、未知 backend、`getItems`/`getTurns`、
  空/数组输入、`maxTurns` 上限、单侧 backend 失败、解码回退）。
- 冻结约束：不改 `event_envelope.rs`（SSE 契约），不改 `memory` 三层结构，不引入
  memo 记忆内部实现。
- 日志沿用 `tracing`，不打印 OpenAI key / 原始用户输入 / 嵌入向量。

## Testing & coverage (do not regress)

* **Gate**: `just cov-check` (`scripts/coverage.sh`) must stay at 100% of lcov
  `DA:` lines outside `docs/coverage.md`. CI runs fmt + `clippy -D warnings` +
  `cargo test --workspace` + the gate.
* **Exclusions are a budget, not a habit**: `aria-agent-ffigen`, cloud `main.rs`,
  `store/pg.rs`, `store/pg_orchestration.rs`, `context/pg.rs`. Adding business logic to that
  list requires editing `docs/coverage.md` with a reason that is not "hard".
* **No live dependencies**: `wiremock`/loopback for HTTP, an in-crate TCP stub for
  the Docker Engine API (`DockerSandbox::with_client`), `store/memory.rs` for the
  metadata store. `DATABASE_URL` tests and the memo `cli-interop` tests stay gated
  and must never fake a pass.
* **Coverage-friendly structure**: one shared error mapper (`db_err`,
  `model_err`, `prefixed`) instead of a `map_err(|e| ...)` closure per call site;
  handlers talk to the `*Store` traits, never to `sqlx` directly.
* **Never** use `assert!(true)`, execution-only calls or `#[ignore]` to move the
  number.

## Common commands

* `just build` — build all crates
* `just test` — `cargo test --workspace`
* `just cov-check` — coverage gate: 100% of lcov line records outside the
  exclusions in `docs/coverage.md` (CI job `coverage`)
* `just cov` — report only (writes `lcov.info`)
* `just sdk-js-test` / `just sdk-py-test` — SDK tests
* `just ffi` — regenerate Swift/Kotlin bindings
* `just cloud` — run the cloud service (needs Postgres **with pgvector**;
  `aria-agent serve` is the default subcommand). `aria-agent serve --port <n>`
  (default `CLOUD_PORT` or 3000).
* `aria-agent setup` / `aria-agent upgrade [version] [--url <url>]` — CLI
  self-management (`~/.ariacompute/agent-cli.yml`).
* `just migrate` — `sqlx database create` + `sqlx migrate run`
* `just fmt` / `just lint` — formatting and clippy (`-D warnings` in CI)
* Release & publish: `.github/workflows/release.yml` (binary + cdylib assets),
  `publish-cargo.yml` (crates.io: sandbox → plugin → core → tools → memo →
  browser → ariacompute-agent; orchestrator/cloud/ffigen are not published),
  `publish-maven.yml` (Maven Central), `publish-npm.yml` (OIDC),
  `publish-pypi.yml` (`PYPI_TOKEN`); Swift via CocoaPods
  `bindings/swift/AriaComputeAgent.podspec`.

Windows build (release matrix `windows-x86_64`, plus the CI `test` matrix): a
previous release failure was caused by unused-variable warnings in
`aria-agent-core`'s `#[cfg(unix)]` tests; those bindings are named `_dir` / `_fs`
and must not be renamed back. The `pty` and `web` features are **off by default**
so mobile builds do not compile them, and no crate declares a direct `winapi`
dependency.
