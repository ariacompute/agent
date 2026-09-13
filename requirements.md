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
* Unified context memory store (`agent-memo`), local/embedded (sled),
  **not Postgres**.
* API: `memorize` / `recall` / `compact`, plus keyed long-term memory.
* The **only** source of conversational/long-term context for `agent-core`
  (run loop) and `agent-cloud` (request construction).
