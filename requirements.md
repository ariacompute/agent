# Requirements

Six modules compose the agent platform. Each maps to a crate/deliverable.

## 1. codex submodule
* `https://github.com/openai/codex.git` is a git submodule at `codex/`.
* Pinned to a stable release tag (not `main`) for reproducibility.
* Not a Cargo workspace member; our crates reference codex's shapes by design.

## 2. Agents Cloud API (Rust + Postgres + pgvector)
* axum HTTP service exposing the **OpenAI beta Agents** resource surface:
  * `POST /v1/agents`, `GET /v1/agents`, `GET /v1/agents/{id}`,
    `POST /v1/agents/{id}` (update), `DELETE /v1/agents/{id}`
  * `POST /v1/agents/sessions`, `GET /v1/agents/sessions`,
    `GET /v1/agents/sessions/{id}`, `POST /v1/agents/sessions/{id}` (update),
    `DELETE /v1/agents/sessions/{id}`
  * `POST /v1/agents/sessions/{id}/events` (blocking turn),
    `POST /v1/agents/sessions/{id}/events/stream` (SSE)
  * read-only `…/items`, `…/turns`, `…/subagents`
  * `501` for `vaults`, `agents/environments` (+files/templates) and
    `…/artifacts`
* Requests accept `OpenAI-Beta: agents=v1`. Auth is Bearer / ApiKey +
  Postgres `api_keys` (sha256), multi-tenant via `principal_id`.
* Calls the model through `aria-agent-core`'s `OpenAiModel` (feature `openai`);
  `OPENAI_BASE_URL` / `OPENAI_API_KEY` configure it.
* **Streaming contract (frozen).** `agent.turn.created` →
  `agent.turn.in_progress` → `agent.turn.item.added` / `agent.turn.item.done` →
  `agent.turn.output_text.delta` → `agent.turn.output_text.done` →
  `agent.turn.completed` (terminal) or `agent.turn.failed` on error. Each frame
  carries `event_id`, `session_id`, `turn_id`, `sequence_number`; there is no
  `[DONE]` sentinel. Mapping lives in
  `crates/aria-agent-cloud/src/event_envelope.rs`.
* **Context memory**: Postgres + pgvector (`context_fragments`,
  `vector(256)`, HNSW cosine index), sharded per principal, hybrid
  vector + keyword recall. pgvector missing ⇒ startup fails (ADR-0010).
* **Default agent.** `ensure_schema` seeds `id: agent-demo` /
  `name: Agent Demo` (admin-owned) so a fresh deployment is usable immediately.

## 3. Agent SDKs
* **JS**: `sdk/js` → npm `@ariacompute/agent`. Mirrors `@openai/agents`:
  `Agent`, `run`, `runStreamed`, `tool`, `Session`, `result.finalOutput`,
  `result.history`.
* **Python**: `sdk/python` → pip `ariacompute-agent`. Mirrors `openai-agents`:
  `Agent`, `Runner.run`, `Runner.run_streamed`, `function_tool`, `Session`,
  `result.final_output`, `result.to_input_list()`.
* Both are thin clients over the beta Agents REST API (no duplicated agent loop);
  tool *schemas* are forwarded, tool *execution* stays in the cloud sandbox.
* **Memory backend switch**: every SDK accepts `cloud` / `local` / `both`
  (constructor config + per-call override); `local` is aria memo.

* **Native SDK (UniFFI, Swift / Kotlin)**: stable FFI surface — `SdkAgent`
  (`run`, `run_stream`, `session`, `session_id`), `SdkSession`
  (`memorize`, `recall`), `create_agent`, `create_agent_with`,
  `create_agent_for_tenant[_with]`, `SdkAgentConfig`, `SdkError`,
  `SdkStreamEvent`, `SdkAgentListener`; compiled to `libaria-agent_ffi`.
  Regenerate bindings with `just ffi` (ADR-0002 / ADR-0008).

### 3.1 SDK 行为契约（js / python 对齐）

- **类型化错误**：传输失败归一为 `AriaError`，`kind ∈ {auth, network, api,
  config, unknown}`；401/403 → `auth`，其他非 2xx → `api`（带 HTTP `status`），
  连接失败 → `network`，本地误用（`maxTurns` 非正整数、缺 `fetch`）→ `config`。
- **`Session` 只读端点**：`getItems()` / `getTurns()` 走 GET `…/items`、
  `…/turns`（规则 8，不改动上下文）；`memorize`/`recall` 走 cloud/local/both。
- **`maxTurns`**：`run`/`runStreamed` 接受正整数上限；`runStreamed` 按
  `agent.turn.created` 计数，超过则早退并抛 `config` 型错误（云端控制多步执行，
  此仅作流防护）。
- **`Agent` 校验**：`name` 必填非空；`tools` 名称唯一（重复抛 `duplicate tool
  name`）；`toolSchemas()` 不透传 `execute`，默认 `parameters`。
- **记忆三态**：`cloud`（agent-cloud REST）/ `local`（aria memo；js 经
  `aria-memo` CLI、python 直连 SQLite `memories` 表）/ `both`（双侧写入、读取
  合并、cloud 优先、local 兜底，单侧失败可容忍）。
- **离线可测**：js 注入 `fetch`、python mock `transport._request`，零网络/零 LLM
  覆盖正常 + 异常路径；不改 `event_envelope.rs`（冻结 SSE 契约），不引入 memo
  记忆内部实现。

## 4. Pluggable Sandbox (docker / kata / cubesandbox / codex / browser)
* `Sandbox` trait (`spawn` / `exec` / `destroy`) with `ExecSpec` / `ExecOutput`
  mirroring codex's `sandboxing`.
* Providers: `DockerSandbox` (default), `KataSandbox`, `CubeSandbox`;
  config-driven selection.
* **`CodexSandbox`** is provided by the `aria-agent-cloud` runtime (ADR-0005) and
  is the backend the cloud selects for agentic tool execution.
* **Browser sandbox** (`aria-agent-browser`): reuses the same Docker/Kata
  container path and `SandboxResourceLimits` via a `BrowserSandbox` that
  implements both `Sandbox` and `BrowserEngine`. A `BrowserCatalog` registers six
  browser images — `servo`, `obscura`, `chromium`, `gosub`, `camoufox`,
  `lightpanda` — each a separate sandbox container image: the Playwright family
  (Chromium / Camoufox / Lightpanda) shares a base image plus a per-engine runner
  (Chromium wires up `playwright-captcha`; Obscura is a Chromium stealth layer),
  while Servo / Gosub are minimal native-engine images. Images are overridable by
  `ARIACOMPUTE_BROWSER_IMAGE_<KIND>`. The agent can **dynamically select /
  switch** the active browser image at runtime via `sandbox_provider = "browser"`
  (default kind `chromium`) and the `browser_use` tool (destroys the current
  container, re-pulls the chosen image, safe rollback on failure). Browser
  capabilities (navigate / extract / click / fill / screenshot / evaluate /
  solve_captcha) are exposed as a browser tool family routed through a
  `ToolHandler` before the shell fallback; native engines degrade unsupported ops
  to `BrowserError::Unsupported` (ADR-0012).

## 5. Context memory (shared contract)
* Contract in `aria-agent-core::context`: `ContextStore` (`memorize`, `recall`,
  `compact`, `get_by_key`), `ContextFragment`, `FragmentKind` (`Message` /
  `ToolResult` / `LongTerm` / `Note`), `RecallQuery`, `EMBED_DIM = 256`.
* Local hashing-trick `LocalEmbedder` (TF + L2 normalized, zero ML deps);
  ranking blends `0.7 * cosine + 0.3 * keyword`, exact key matches rank first.
* Cloud: `PgContextStore` (pgvector KNN + keyword candidates, tenant scoped).
  Local: `MemoContextStore` (aria memo / SQLite, offline-first). `both`:
  `CompositeContextStore` (double write + merged, deduped reads).
* Covered by unit tests for normal paths (roundtrip, kind/session filtering,
  embedding population, semantic ranking, tenant isolation) and abnormal paths
  (empty query, missing session → `NotFound`, empty embedding text).
* **Run-loop memo integration** (`aria-agent-core`): the agentic loop now goes
  beyond `recall`/`memorize`. Each turn (a) supplements vector recall with bounded
  graph `expand` from the recalled fragment seeds, (b) relates the turn's
  user↔assistant fragments with `RelationKind::Temporal` edges, and (c) auto-`compact`s
  the session once its fragment count exceeds `AgentConfig::memory_compact_threshold`.
  A long-term-memory tool family — `memo_store` / `memo_get` / `memo_search` /
  `memo_related` — is exposed via `MemoToolHandler` (a `ToolHandler` routed before
  the shell fallback), so the model can actively read/write its own memory. Both the
  graph and auto-compact are gated by `AgentConfig::memory_graph` (off by default in
  core for hermetic tests; the SDK enables it, `memory_graph = true`,
  `threshold = 32`). `aria-agent-core` keeps a hard dependency-inversion boundary: it
  depends only on the `ContextStore` trait, never on `aria-agent-memo`, so the local
  (aria memo / SQLite) and cloud (pgvector) backends stay interchangeable. See
  `docs/adr/0013-agent-memo-integration.md`.

## 6. Release & publishing
* On `release: created`, `.github/workflows/release.yml` builds/tests/packages
  the `aria-agent-cloud` binary and the `ariacompute-agent` FFI cdylib across
  linux-x86_64 / windows-x86_64 / linux-arm64 / macos and uploads them to the
  GitHub Release (`secrets.ARIACOMPUTE_TOKEN`).
* `publish-cargo.yml` publishes to crates.io in topological order
  `aria-agent-sandbox` → `aria-agent-core` → `aria-agent-memo` →
  `ariacompute-agent` (`secrets.CARGO_REGISTRY_TOKEN`).
* `publish-maven.yml` publishes the Kotlin/Android binding to Maven Central
  (`com.ariacompute:agent`) via vanniktech (`secrets.SONATYPE_*`,
  `secrets.GPG_*`).
* `publish-npm.yml` publishes `@ariacompute/agent` (`secrets.NPM_TOKEN`);
  `publish-pypi.yml` publishes `ariacompute-agent` (`secrets.PYPI_API_TOKEN`).
* Swift is published via CocoaPods from `bindings/swift/AriaComputeAgent.podspec`
  (`secrets.COCOAPODS_TRUNK_TOKEN`).

## 7. Removed features
* **Reef self-improvement** (record → feedback → evolve → version → hot-serve)
  has been removed: the `aria-agent-reef` crate, `/reef/*` routes, the
  `x-reef-agent-record-id` header and the hot-swappable harness are gone.
  The system prompt is now static (agent name + optional `instructions`).
* The old `/v1/sessions/:id/runs[/stream]` routes and the `response.*` envelope
  are removed (strict switch to the beta Agents API, ADR-0009).

## 8. Downstream follow-ups (out of scope for this repo)
Tracked in `docs/followups/`:
* `playground-migration.md` — proxy routes + SSE parsing + sandbox env.
* `serve-copy-migration.md` — marketing copy / endpoint examples / SEO build.
* `cockpit-client-migration.md` — `CloudClient.kt` endpoint + frame parsing,
  native memo signature re-verification.
