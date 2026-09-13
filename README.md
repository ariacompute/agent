# agent

A layered agent platform built on OpenAI's [`codex`](https://github.com/openai/codex)
harness, exposing agents as (a) a **Rust + Postgres cloud API** and (b)
**native SDKs for Swift / Kotlin** via a UniFFI boundary.

## Modules

| Module | What it is |
|--------|------------|
| **codex submodule** | `openai/codex` at `codex/` (harness, sandboxing, memories). |
| **agent-memo** | Unified **context memory** (memo). Local/embedded store, **not Postgres**. |
| **agent-sandbox** | Pluggable `Sandbox`: Docker (default) / Kata / Cube. |
| **agent-core** | Unified agent runtime: `recall → model → memorize`, tools in a sandbox. |
| **agent-sdk** | UniFFI `cdylib` (`libagent_sdk`): `SdkAgent` / `SdkSession` / `create_agent`. |
| **agent-cloud** | axum service, Postgres metadata only, OpenAI Agents API. `/v1/agents`, `/v1/runs`, SSE `/v1/runs/stream`. |
| **bindings** | `bindings/swift` (SwiftPM) and `bindings/kotlin` (Android). |

> **Storage boundary:** memo is the *only* context store and never uses
> Postgres. Postgres is used *only* by `agent-cloud` for `agents`/`runs`
> metadata.

## Quick start

```bash
# 1. codex submodule
git submodule update --init --depth 1 codex

# 2. build everything
cargo build --workspace

# 3. test
cargo test --workspace

# 4. generate Swift/Kotlin bindings (needs the cdylib built)
just ffi        # = cargo run -p agent-ffigen

# 5. run the cloud API (needs Postgres + OpenAI key)
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/agent
export OPENAI_API_KEY=sk-...
cargo run -p agent-cloud
```

## Using the cloud API

```bash
curl -X POST localhost:3000/v1/agents -d '{"name":"my-agent"}'
curl -X POST localhost:3000/v1/runs   -d '{"agent_id":"<id>","session":"s1","input":"hello"}'
```

## Using the native SDK (Swift / Kotlin)

See `bindings/swift/README.md` and `bindings/kotlin/agent-sdk/README.md`.

## Layout

```
crates/      agent-memo, agent-sandbox, agent-core, agent-sdk, agent-cloud, agent-ffigen
bindings/    swift, kotlin
codex/       openai/codex submodule
docs/        architecture.md + adr/
migrations/  Postgres metadata schema
tests/       cross-crate integration tests
```

## Engineering Conventions

This repository follows the Harness Engineering philosophy:

- [`AGENTS.md`](AGENTS.md): Agent engineering context entry and directory index
- [`requirements.md`](requirements.md): Requirements spec (feature boundaries/exceptions/acceptance criteria, human-review-gated)
- [`task.md`](task.md): Implementation task checklist

## License

MIT.
