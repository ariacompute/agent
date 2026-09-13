# AGENTS.md — contribution & architecture conventions

This repo is a Rust workspace that wraps the OpenAI **codex** harness and ships a
cloud API plus native (Swift/Kotlin) SDKs.

## Workspace layout

* `crates/agent-memo`, `agent-sandbox`, `agent-core`, `agent-sdk`,
  `agent-cloud`, `agent-ffigen`
* `bindings/swift`, `bindings/kotlin` (generated; do not edit by hand)
* `codex/` is a **git submodule** (do not add it to the Cargo workspace).

## Rules

1. **memo is the only context store.** Never persist conversational/long-term
   context in Postgres. Postgres (`agent-cloud`) holds metadata only
   (`agents`, `runs`). See `docs/adr/0004-memo-storage.md`.
2. **FFI is stable.** Only change `agent-sdk`'s exported surface deliberately.
   After any change run `just ffi` and commit the regenerated bindings. See
   `docs/adr/0002-ffi-boundary.md`.
3. **Sandbox is pluggable.** Add providers by implementing the `Sandbox` trait
   and registering them in `from_provider`. Docker is the default. See
   `docs/adr/0003-sandbox-providers.md`.
4. **Submodule discipline.** Keep `codex` out of the workspace member list;
   reference codex crate shapes by design. See `docs/adr/0001-submodule-strategy.md`.
5. **Secrets/logging.** Use `tracing`. Never log the OpenAI key or raw user
   content.
6. **Tests.** `cargo test --workspace` must pass. Cross-crate behavior belongs
   in `tests/`.

## Common commands

* `just build` — build all crates
* `just test` — run tests
* `just ffi` — regenerate Swift/Kotlin bindings
* `just cloud` — run the cloud service
* `just fmt` / `just lint` — formatting and clippy
