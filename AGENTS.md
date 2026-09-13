# AGENTS.md — contribution & architecture conventions

This repo is a Rust workspace that wraps the OpenAI **codex** harness and ships a
cloud API plus native (Swift/Kotlin) SDKs.

## Workspace layout

* `crates/agent-memo`, `agent-sandbox`, `agent-core`, `agent-sdk`,
  `agent-cloud`, `agent-reef`, `agent-ffigen`
* `bindings/swift`, `bindings/kotlin` (generated; do not edit by hand)
* `codex/` is a **git submodule** (do not add it to the Cargo workspace).

## Rules

1. **memo is the only context store.** Never persist conversational/long-term
   context in Postgres. Postgres (`agent-cloud`) holds metadata only
   (`agents`, `runs`). See `docs/adr/0004-memo-storage.md`. `agent-memo` is a
   sled-backed `MemoStore` (`memorize` / `recall` / `compact` / `get_by_key`)
   with `ContextFragment` (`key` + `FragmentKind` + optional dense `embedding`)
   and local hashing-trick + cosine semantic recall; its unit tests cover
   normal + abnormal paths (`cargo test -p agent-memo`).
2. **FFI is stable.** Only change `agent-sdk`'s exported surface deliberately.
   After any change run `just ffi` and commit the regenerated bindings. See
   `docs/adr/0002-ffi-boundary.md`.
3. **Sandbox is pluggable.** Add providers by implementing the `Sandbox` trait
   and registering them in `from_provider`. Docker is the default. See
   `docs/adr/0003-sandbox-providers.md`.
4. **Submodule discipline.** Keep `codex` out of the workspace member list;
   reference codex crate shapes by design. Deep compile-integration of codex's
   `sandboxing`/`memories` crates is prescribed in
   `docs/adr/0005-codex-integration.md` (currently blocked by a broken upstream
   dependency; our `agent-sandbox`/`agent-memo` are the self-contained
   integration seam). See `docs/adr/0001-submodule-strategy.md`.
5. **Secrets/logging.** Use `tracing`. Never log the OpenAI key or raw user
   content.
6. **Tests.** `cargo test --workspace` must pass. Cross-crate behavior belongs
   in `tests/`.
7. **Cloud auth + streaming.** `agent-cloud` is gated by `AGENT_CLOUD_API_KEY`
   (send `Authorization: Bearer <key>` or `ApiKey <key>`; open when unset) and
   serves token streaming at `POST /v1/runs/stream` (SSE, terminated by
   `[DONE]`).
8. **Reef stores are separate from context/metadata.** `agent-reef` logs every
   turn (`RecordStore`) and binds feedback (`FeedbackStore`) in a **local sled
   DB**, and versions winning harnesses in a **`.reef/` Git repo** — never in
   memo (context store) nor Postgres (metadata only). The served harness is a
   shared `ActiveHarness` hot-swapped atomically (no restart). See
   `docs/adr/0006-reef-self-improvement.md`. Cloud routes: `POST /reef/report`,
   `POST /reef/evolve`, `GET /reef/versions`.

## Common commands

* `just build` — build all crates
* `just test` — run tests
* `just reef-test` — run `agent-reef` + integration tests (no API key / Postgres)
* `just cov` — `cargo tarpaulin` coverage (if available)
* `just ffi` — regenerate Swift/Kotlin bindings
* `just cloud` — run the cloud service
* `just fmt` / `just lint` — formatting and clippy
