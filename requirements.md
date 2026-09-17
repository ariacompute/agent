# Requirements

Five modules compose the agent platform. Each maps to a crate/deliverable.

## 1. codex submodule
* `https://github.com/openai/codex.git` is a git submodule at `codex/`.
* Pinned to a stable release tag (not `main`) for reproducibility.
* Not a Cargo workspace member; our crates reference codex's shapes by design.

## 2. Agents Cloud API (Rust + Postgres)
* axum HTTP service exposing `/v1/agents` (create / get / **list**) and
  `/v1/sessions/:id/runs` (JSON) plus SSE-streaming `/v1/sessions/:id/runs/stream`.
* Calls the OpenAI Agents / Responses API through `aria-agent-core`'s
  `OpenAiModel` (feature `openai`). `OPENAI_BASE_URL` overrides the endpoint
  so a deployment can target any OpenAI-compatible gateway; `OPENAI_API_KEY`
  supplies the credential.
* **Postgres stores only metadata** (`agents`, `runs`); context is injected
  from memo.
* **Streaming contract (frozen, OpenAI-compatible).** `/v1/sessions/:id/runs/stream`
  emits OpenAI Responses API streaming events — `response.created`,
  `response.in_progress`, `response.output_item.added`,
  `response.function_call_arguments.delta`, `response.output_text.delta`,
  `response.output_text.done`, `response.output_item.done`,
  `response.completed`, `response.failed` — each with a monotonic
  `sequence_number`; `response.completed` (or `response.failed`) is the only
  terminal event — there is no trailing `data: [DONE]` sentinel. No private
  `aria.*` frames:
  the agentic phase (`recall` / `model` / `tool_exec` / `loop_guard`), the
  executed tool `result` and the Reef receipt `reef_record_id` ride along as
  extra fields inside OpenAI-shaped frames. The mapping lives in
  `crates/aria-agent-cloud/src/event_envelope.rs`. See the
  "OpenAI Agents API compatibility" section of `README.md` / `README_cn.md`.
* **Agentic runs.** `run_agent_stream` drives
  `Agent::run_event_stream(input, agent_tools())`: a single `shell` tool
  (`command` = `[program, ...args]`) executed by the **codex** sandbox backend
  (ADR-0005), with the loop guard and memo persistence handled by
  `aria-agent-core`.
* **Default agent.** `ensure_schema` seeds `id: agent-demo` /
  `name: Agent Demo` (admin-owned) so a fresh deployment is usable
  immediately; a pre-existing legacy `playground-demo` row is renamed in
  place. Both statements are idempotent.

## 3. Agent SDK (UniFFI, Swift / Kotlin)
* Stable FFI surface: `SdkAgent` (`run`, `session`), `SdkSession`
  (`memorize`, `recall`), `create_agent`, `SdkAgentConfig`, `SdkError`.
* Compiled to a cross-platform `cdylib` (`libaria-agent_ffi`).
* Swift (SwiftPM) and Kotlin (Android) bindings generated from the cdylib.
* Both binding READMEs additionally document **cloud streaming**
  (OpenAI-compatible): consuming `/v1/sessions/:id/runs/stream` `response.*` events from
  Swift / Kotlin over HTTP. This is documentation only — the FFI surface
  itself is unchanged and needs no `just ffi` regeneration.

## 4. Pluggable Sandbox (docker / kata / cubesandbox / codex)
* `Sandbox` trait (`spawn` / `exec` / `destroy`) with an `ExecSpec` /
  `ExecOutput` shape mirroring codex's `sandboxing`.
* Providers: `DockerSandbox` (default), `KataSandbox`, `CubeSandbox`;
  config-driven selection.
* **`CodexSandbox`** is provided by the `aria-agent-cloud` runtime (it depends
  on codex's git-only crates, so it must stay out of the published
  `aria-agent-sandbox`). It runs commands through codex's real
  `SandboxManager` and is the backend the cloud selects for agentic tool
  execution. See `docs/adr/0005-codex-integration.md`.

## 5. Memo (Context Memory)
* Unified context memory store (`aria-agent-memo`), local/embedded (**sled**,
  `:memory:` for tests), **not Postgres**.
* `MemoStore` trait contract: `memorize` / `recall` / `compact` /
  `get_by_key`. `SledMemoStore` is the shipped implementation.
* `ContextFragment` carries `session`, optional `key` (keyed long-term
  memory), `FragmentKind` (`Message` / `ToolResult` / `LongTerm` / `Note`),
  `content`, and an optional dense `embedding`.
* `recall` takes a `RecallQuery` (`session` / `text` / `top_k` / optional
  `kind`) and blends keyword relevance with **semantic (vector) recall**: a
  local hashing-trick `LocalEmbedder` (TF/L2-normalized, zero ML deps)
  auto-populates `embedding` on memorize and `cosine` similarity ranks
  results; exact `key` matches rank first.
* The **only** source of conversational/long-term context for `aria-agent-core`
  (run loop) and `aria-agent-cloud` (request construction).
* Covered by unit tests for normal (memorize/recall roundtrip, kind + session
  filtering, `get_by_key` roundtrip, embedding population, semantic ranking)
  and abnormal paths (empty query, `compact` on a missing session →
  `NotFound`, embedder on empty text).

## 6. Reef self-improvement (record → feedback → evolve → version → hot-serve)
* `aria-agent-reef` crate implements a Reef-style continuous self-improvement loop
  (inspired by Human-Agent-Society/reef). It evolves the agent's **skills /
  prompts / rules** (the `Harness`), not its long-term context.
* **Record**: every turn is logged to a local sled `RecordStore`, returning a
  record id. The cloud surfaces it as the `x-reef-agent-record-id` response
  header (on both JSON and SSE responses).
* **Feedback**: `FeedbackStore` (sled) binds `(score, references=record ids)` to
  records. Eligibility = enough feedback volume **or** any negative signal.
* **Evolve**: `EvolutionEngine` asks the pluggable local model engine
  (`ModelClient`, the "local engine FFI") for a candidate `Harness`
  (`system_prompt` + `skills` + `rules`), scores it against the baseline, and
  **keeps only the winner** (no regression).
* **Version**: the winning harness is persisted as Markdown under `.reef/` and
  committed / tagged (`reef@<n>`) via `std::process::Command` (no `git2` dep);
  **fail-closed** when git is unavailable or the commit fails.
* **Hot-serve**: the served harness is a shared `ActiveHarness`
  (`Arc<RwLock<Arc<Harness>>>`) — `aria-agent-core`'s `Agent` reads it each turn
  (O(1) `Arc` clone, no serialization, no restart). `aria-agent-cloud` loads the
  last winner on boot (fallback to baseline) and shares it across all agents.
* Cloud endpoints: `POST /reef/report` (bind feedback), `POST /reef/evolve`
  (run the loop), `GET /reef/versions` (list Git tags).
* Records / feedback / versions live in **local sled + `.reef/`** — never in
  memo (context store) nor Postgres (metadata only). See
  `docs/adr/0006-reef-self-improvement.md`.

## 7. Release & publishing
* On `release: created`, `.github/workflows/release.yml` builds, tests, and packages
  the `aria-agent-cloud` binary and the `ariacompute-agent` FFI cdylib (`libaria-agent_ffi`) across
  linux-x86_64 / windows-x86_64 / linux-arm64 / macos, injects the release tag (sans
  leading `v`) via `ARIA_AGENT_VERSION`, and uploads the archives to the GitHub Release
  (`secrets.ARIACOMPUTE_TOKEN`). `aria-agent-cloud` is the CLI/cloud binary (GitHub Release
  asset, not on crates.io); `aria-agent-ffigen` is `publish = false`.
* Language-package publishing is a **fail-pass** `publish-packages` job
  (`continue-on-error: true`) that only stubs crates.io / CocoaPods Swift /
  Maven Central so it can never block the CLI/FFI assets.
* `publish-cargo.yml` publishes to crates.io in topological order:
  `aria-agent-memo` → `aria-agent-sandbox` → `aria-agent-core` → `ariacompute-agent`, with version injection
  into the workspace `Cargo.toml` and retries for registry lag / HTTP 429
  (`secrets.CARGO_REGISTRY_TOKEN`).
* `publish-maven.yml` publishes the Kotlin/Android binding to Maven Central
  (`com.ariacompute:agent`) via the vanniktech plugin
  (`bindings/kotlin/ariacompute-agent/build.gradle.kts`): `publishToMavenCentral` +
  `signAllPublications()`, using `secrets.SONATYPE_USERNAME` / `SONATYPE_PASSWORD` and
  `secrets.GPG_PRIVATE_KEY` / `GPG_PASSPHRASE`.
* Swift is published via CocoaPods from `bindings/swift/AriaAgent.podspec`
  (`pod trunk push`; `secrets.COCOAPODS_TRUNK_TOKEN`), wrapping the native
  `libaria-agent_ffi.a` built by `cargo build -p ariacompute-agent`.
