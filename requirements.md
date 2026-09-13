# Requirements

Five modules compose the agent platform. Each maps to a crate/deliverable.

## 1. codex submodule
* `https://github.com/openai/codex.git` is a git submodule at `codex/`.
* Pinned to a stable release tag (not `main`) for reproducibility.
* Not a Cargo workspace member; our crates reference codex's shapes by design.

## 2. Agents Cloud API (Rust + Postgres)
* axum HTTP service exposing `/v1/agents` (create/get) and `/v1/runs`
  (JSON) plus SSE-streaming `/v1/runs/stream`.
* Calls the OpenAI Agents / Responses API through `agent-core`'s
  `OpenAiModel` (feature `openai`).
* **Postgres stores only metadata** (`agents`, `runs`); context is injected
  from memo.

## 3. Agent SDK (UniFFI, Swift / Kotlin)
* Stable FFI surface: `SdkAgent` (`run`, `session`), `SdkSession`
  (`memorize`, `recall`), `create_agent`, `SdkAgentConfig`, `SdkError`.
* Compiled to a cross-platform `cdylib` (`libagent_sdk`).
* Swift (SwiftPM) and Kotlin (Android) bindings generated from the cdylib.

## 4. Pluggable Sandbox (docker / kata / cubesandbox)
* `Sandbox` trait (`spawn` / `exec` / `destroy`) with an `ExecSpec` /
  `ExecOutput` shape mirroring codex's `sandboxing`.
* Providers: `DockerSandbox` (default), `KataSandbox`, `CubeSandbox`;
  config-driven selection.

## 5. Memo (Context Memory)
* Unified context memory store (`agent-memo`), local/embedded (**sled**,
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
* The **only** source of conversational/long-term context for `agent-core`
  (run loop) and `agent-cloud` (request construction).
* Covered by unit tests for normal (memorize/recall roundtrip, kind + session
  filtering, `get_by_key` roundtrip, embedding population, semantic ranking)
  and abnormal paths (empty query, `compact` on a missing session →
  `NotFound`, embedder on empty text).

## 6. Reef self-improvement (record → feedback → evolve → version → hot-serve)
* `agent-reef` crate implements a Reef-style continuous self-improvement loop
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
  (`Arc<RwLock<Arc<Harness>>>`) — `agent-core`'s `Agent` reads it each turn
  (O(1) `Arc` clone, no serialization, no restart). `agent-cloud` loads the
  last winner on boot (fallback to baseline) and shares it across all agents.
* Cloud endpoints: `POST /reef/report` (bind feedback), `POST /reef/evolve`
  (run the loop), `GET /reef/versions` (list Git tags).
* Records / feedback / versions live in **local sled + `.reef/`** — never in
  memo (context store) nor Postgres (metadata only). See
  `docs/adr/0006-reef-self-improvement.md`.
