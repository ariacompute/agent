# Requirements

Six modules compose the agent platform. Each maps to a crate/deliverable.

## 1. Plugin registry & capability seams
* Every capability is a seam (`llm`, `sandbox`, `fs`, `shell`, `jobs`, `storage`,
  `sessionPersistence`, `sessionQuery`, `approvals`, `skills`, `web`,
  `userQuestions`, `subagents`, `schedule`) with a default provider.
* `aria-agent-plugin` assembles providers from `PluginManifest`s: one provider per
  `core` key, topo-sorted `requires`, duplicate/cycle detection, frozen snapshot.
* Any assembly failure is a boot error (`PluginError`), never a runtime surprise.

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

* **Rust / Swift / Kotlin (UniFFI)**: a **cloud API client**, same surface and
  vocabulary as JS/Python — `AriaAgent` (constructor + `name`/`model`/`id`,
  `createSession`, `session(id)`, `run`, `runStreamed`),
  `Session` (`id`, `memoryBackend`, `memorize`, `recall`, `itemsJson`,
  `turnsJson`, `titleJson`, `historyJson`), `AriaAgentConfig`, `ClientOptions`,
  `MemoryConfig`, `RunResult`, `TokenUsage`, `StreamEvent`, `StreamListener`,
  `AriaError`; compiled to `libaria-agent_ffi`. No runtime, sandbox or model key
  is embedded — requests go to `/v1/agents/...` exactly like the other SDKs.
  Regenerate bindings with `just ffi` (ADR-0002 / ADR-0008 / ADR-0020).

### 3.1 SDK 行为契约（js / python 对齐）

- **类型化错误**：传输失败归一为 `AriaError`，`kind ∈ {auth, network, api,
  config, unknown}`；401/403 → `auth`，其他非 2xx → `api`（带 HTTP `status`），
  连接失败 → `network`，本地误用（`maxTurns` 非正整数、缺 `fetch`）→ `config`。
- **`Session` 只读端点**：`getItems()` / `getTurns()` 走 GET `…/items`、
  `…/turns`（规则 8，不改动上下文）；`memorize`/`recall` 走 cloud/local/both。
- **`maxTurns`**：`run`/`runStreamed` 接受正整数上限；`runStreamed` 按
  `agent.turn.created` 计数，超过则早退并抛 `config` 型错误（云端控制多步执行，
  此仅作流防护）。
- **空流**：`runStreamed` 收到零个 `agent.*` 帧时抛 `unknown` 型错误，绝不返回
  空答案（js / python / Rust / Swift / Kotlin 一致）。
- **`Agent` 校验**：`name` 必填非空；`tools` 名称唯一（重复抛 `duplicate tool
  name`）；`toolSchemas()` 不透传 `execute`，默认 `parameters`。
- **记忆三态**：`cloud`（agent-cloud REST）/ `local`（aria memo；js 经
  `aria-memo` CLI、python 直连 SQLite `memories` 表）/ `both`（双侧写入、读取
  合并、cloud 优先、local 兜底，单侧失败可容忍）。三端 `local` 行统一为
  `key: value`（可被 `aria-memo list` 及任一 SDK 读出）；未知后端 → `config`
  型错误；缺失键返回 `None`，不回落到无关命中。
- **离线可测**：js 注入 `fetch`、python mock `transport._request`，零网络/零 LLM
  覆盖正常 + 异常路径；不改 `event_envelope.rs`（冻结 SSE 契约），不引入 memo
  记忆内部实现。

## 4. Pluggable Sandbox (docker / kata / cubesandbox / browser)
* `Sandbox` trait (`spawn` / `exec` / `destroy`) with `ExecSpec` / `ExecOutput`
  (argv-only commands, resource limits).
* Providers: `DockerSandbox` (default), `KataSandbox`, `CubeSandbox`;
  config-driven selection. An unknown provider is a hard configuration error
  (ADR-0018).
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
  `aria-agent-sandbox` → `aria-agent-plugin` → `aria-agent-core` →
  `aria-agent-tools` → `aria-agent-memo` → `aria-agent-browser` →
  `ariacompute-agent` (`secrets.CARGO_REGISTRY_TOKEN`). The workspace-internal
  `aria-agent-orchestrator`, `aria-agent-cloud`, `aria-agent-ffigen`,
  `aria-agent-dsh`, `aria-agent-governance` and `aria-agent-egress-proxy`
  crates are **not** published: they depend on workspace-internal crates by
  path only (no version requirement), so `cargo package` refuses them. Each
  carries `publish = false` in its manifest so `cargo publish` skips it
  instead of failing the workspace release.
* `publish-maven.yml` publishes the Kotlin/Android binding to Maven Central
  (`com.ariacompute:agent`) via vanniktech (`secrets.SONATYPE_*`,
  `secrets.GPG_*`).
* `publish-npm.yml` publishes `@ariacompute/agent` via npm Trusted Publishing
  (OIDC; no repository secret); `publish-pypi.yml` publishes `ariacompute-agent`
  using `secrets.PYPI_TOKEN`.
* Swift is published via CocoaPods from `bindings/swift/AriaComputeAgent.podspec`
  (`secrets.COCOAPODS_TRUNK_TOKEN`).

## 7. Removed features
* **Reef self-improvement** (record → feedback → evolve → version → hot-serve)
  has been removed: the `aria-agent-reef` crate, `/reef/*` routes, the
  `x-reef-agent-record-id` header and the hot-swappable harness are gone.
  The system prompt is now static (agent name + optional `instructions`).
* The old `/v1/sessions/:id/runs[/stream]` routes and the `response.*` envelope
  are removed (strict switch to the beta Agents API, ADR-0009).

## 9. Testing & coverage requirements

### 9.1 Gate
* `cargo test --workspace` must be green; new behaviour needs a normal-path and an
  abnormal-path test.
* `just cov-check` (`scripts/coverage.sh`) must report **100% line coverage** for
  every file that is not on the exclusion list in `docs/coverage.md`; CI runs it as
  a required job (`.github/workflows/ci.yml`).
* Coverage is measured on lcov `DA:` records (one per source line), not on
  `llvm-cov --fail-under-lines`, which also counts duplicate records emitted for
  macro/generic instantiations.
* `cargo fmt --all -- --check` and
  `cargo clippy --workspace --all-targets -- -D warnings` must pass.

### 9.2 No live dependencies in tests
* No test may reach the public internet, a real Postgres/pgvector instance, or a
  real Docker daemon: HTTP goes to `wiremock`/loopback stubs, the Docker Engine API
  to the in-crate TCP stub, and the metadata store to `store/memory.rs`.
* Tests that genuinely need an external system (the `DATABASE_URL`-gated Postgres
  tests, the `cli-interop` memo tests) stay behind an explicit gate and are
  excluded from the gate above. They must never report a fabricated pass.

### 9.3 Refactor rules that keep the gate honest
* Format error messages in **shared** helpers (`db_err`, `model_err`,
  `prefixed`) instead of one `map_err(|e| ...)` closure per call site; per-site
  closures are unreachable in practice and turn into permanent coverage debt.
* Do not add `assert!(true)`, call-only-for-execution statements, or `#[ignore]`
  to reach the target. An unreachable branch must be removed or listed in
  `docs/coverage.md` with a reason.
* Data access goes through the `AgentStore`/`SessionStore`/`ApiKeyStore` traits so
  handlers stay testable without a database; production SQL stays in
  `store/pg.rs`.

## 8. Downstream follow-ups (out of scope for this repo)
Tracked in `docs/followups/`:
* `playground-migration.md` — proxy routes + SSE parsing + sandbox env.
* `serve-copy-migration.md` — marketing copy / endpoint examples / SEO build.
* `cockpit-client-migration.md` — `CloudClient.kt` endpoint + frame parsing,
  native memo signature re-verification.
## 10. Turn runtime (DeepSeek Harness)
* The turn loop is the `agent_loop` seam: `AgentLoop::run` streams `AgentEvent`s and
  `run_once` folds them; `LoopContext` is a resolved-once snapshot of every
  dependency a turn may touch.
* `AgentConfig::harness` selects `native` or `dsh`. The library default is
  `native`; the cloud binary defaults to `dsh`; an unknown value, or `dsh` without
  an injected loop, is a hard error (never a silent fallback).
* `harness = "dsh"` runs `dsh --profile sdk` as a child process and speaks
  newline-delimited JSON-RPC on stdio (no third-party gateway plugin). Frames are
  capped, stderr is drained, requests time out, and a runtime that dies fails the
  turn instead of yielding an empty answer.
* Under `dsh`, tools execute in dsh's sandbox: the aria `Sandbox`,
  `ToolExecutionPipeline` and registered handlers do not run for that session.
  aria still owns recall, memory writes, compaction, session events/titles,
  approval/question bridging and the frozen wire envelope.
* Approvals and user questions are answered from the installed seams and fail
  closed when absent.
* Each session gets its own `DSH_HOME` and workspace; the child environment is an
  allow-list; `~/.dsh` is never discovered. Runtimes are pooled per session with a
  hard capacity and an idle TTL.
* Seam contract matches dsh: roles `core`/`seam`/`bundle`/`service`, dsh spellings
  accepted as aliases, registration order = precedence, `dispose` releases in
  reverse assembly order, `service` keys reject providers.
* Verification: offline fake-transport/scripted-runtime tests, a scripted real
  child process (`just dsh-smoke`), and one env-gated live turn (`just dsh-e2e`)
  that skips loudly instead of fabricating a result.
