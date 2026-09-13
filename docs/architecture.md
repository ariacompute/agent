# Architecture

A layered agent platform built on top of the OpenAI **codex** harness, exposed
as (a) a Rust + Postgres cloud service and (b) native SDKs for Swift / Kotlin.

## Layers

```
codex submodule (codex-rs: core/harness, sandboxing, memories)
        │  (referenced by design; our crates mirror its shape)
        ▼
agent-core        unified agent runtime (run loop + tool scheduling)
        │
   ┌────┴─────────────┬───────────────────────┐
   ▼                  ▼                       ▼
agent-sandbox    agent-memo (Context Memory)   (shared cross-cutting)
   │                  │
   ▼                  ▼
agent-cloud  ◀──── memo + sandbox (context + isolation)
   │  HTTP API
   ▼
agent-sdk (UniFFI cdylib) ──► bindings/swift, bindings/kotlin
```

| Crate | Role |
|-------|------|
| `agent-memo` | Unified **context memory** (memo). Local/embedded store (sled). `memorize` / `recall` / `compact`. **Not Postgres.** |
| `agent-sandbox` | Pluggable `Sandbox` trait. Providers: `DockerSandbox` (default), `KataSandbox`, `CubeSandbox`. |
| `agent-core` | `Agent` runtime. Every `run` does `recall` → model → `memorize`. Tools run in a `Sandbox`. |
| `agent-sdk` | UniFFI `cdylib` (`libagent_sdk`). Stable FFI: `SdkAgent`, `SdkSession`. |
| `agent-cloud` | axum service. Postgres holds **only metadata** (`agents`, `runs`); context comes from memo. Calls OpenAI via `agent-core`. |
| `agent-ffigen` | Helper that regenerates Swift/Kotlin bindings from the cdylib. |

## Storage boundary (important)

* **memo** is the *only* source of conversational / long-term context. It uses a
  local/embedded store and **never** Postgres.
* **Postgres** is used *exclusively* by `agent-cloud` for structured metadata.
  Context text is never written to Postgres (see `migrations/0001_init.sql`).

## Agent run loop (in `agent-core::Agent::run`)

1. `recall` context fragments from memo for the session.
2. Persist the user turn to memo.
3. Call the model (`ModelClient`): `StubModel` by default, `OpenAiModel`
   (feature `openai`) for the real OpenAI Responses/Chat API.
4. Persist the assistant reply to memo.
5. Return the reply.

## Sandbox

`agent-sandbox` mirrors codex's `sandboxing` / `linux-sandbox` `ExecSpec` /
`ExecOutput` shape and adds a `SandboxProvider` abstraction. Each provider shells
out to a platform binary (`docker`, `docker --runtime=kata`, `cube`). Docker is
the default; selectable via `AgentConfig.sandbox_provider`.

## FFI

`agent-sdk` exposes a small stable surface (`SdkAgent`, `SdkSession`,
`create_agent`). Generate bindings with `just ffi` (runs `agent-ffigen`), which
emits `bindings/swift` and `bindings/kotlin`.

## References to codex

`codex/` is a git submodule. We deliberately do **not** pull `codex-rs` into this
workspace as a member; our crates reference codex's harness/sandbox/memories
*by design* (same shapes) and can be extended to depend on specific codex crates
via precise path dependencies when deeper integration is needed.
