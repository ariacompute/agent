# AGENTS.md — contribution & architecture conventions

This repo is a Rust workspace that wraps the OpenAI **codex** harness and ships a
cloud API (OpenAI **beta Agents** compatible) plus native (Swift/Kotlin) and
language (JS/Python) SDKs.

## Workspace layout

* `crates/aria-agent-core`, `aria-agent-sandbox`, `aria-agent-memo` (aria memo),
  `ariacompute-agent`, `aria-agent-cloud`, `aria-agent-ffigen`
* `bindings/swift` (`Package.swift` SwiftPM + `AriaAgent.podspec` CocoaPods),
  `bindings/kotlin` (Android `build.gradle.kts` with vanniktech Maven publish).
  Generated Swift/Kotlin *sources* are committed and must not be edited by hand —
  regenerate via `just ffi`; the podspec and Gradle publishing config are hand-maintained.
* `sdk/js` — npm package `@ariacompute/agent` (mirrors `@openai/agents`).
* `sdk/python` — pip package `ariacompute-agent` (mirrors `openai-agents`).
* `codex/` is a **git submodule** (do not add it to the Cargo workspace).
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
2. **FFI is stable.** Only change `ariacompute-agent`'s exported surface
   deliberately. After any change run `just ffi` and commit the regenerated
   bindings. See `docs/adr/0002-ffi-boundary.md`. The public SDK ergonomics
   (session-less happy path) are specified in `docs/adr/0008-sdk-interface.md`.
3. **Sandbox is pluggable.** Add providers by implementing the `Sandbox` trait
   and registering them in `from_provider`. Docker is the default. The cloud
   selects the **codex** backend (`sandbox_provider = "codex"`), resolved by this
   runtime to `codex_sandbox::CodexSandbox` (ADR-0005); Docker/Kata/Cube remain
   available through `from_provider`. See `docs/adr/0003-sandbox-providers.md`
   and `docs/adr/0005-codex-integration.md`.
4. **Submodule discipline.** Keep `codex` out of the workspace member list;
   reference codex crate shapes by design. See `docs/adr/0001-submodule-strategy.md`.
5. **Secrets/logging.** Use `tracing`. Never log the OpenAI key, raw user content,
   or embedding vectors.
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
   `…/turns`, `…/subagents`. `vaults`, `agents/environments` (+files/templates)
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

## Common commands

* `just build` — build all crates
* `just test` — `cargo test --workspace`
* `just sdk-js-test` / `just sdk-py-test` — SDK tests
* `just ffi` — regenerate Swift/Kotlin bindings
* `just cloud` — run the cloud service (needs Postgres **with pgvector**;
  `aria-agent serve` is the default subcommand). `aria-agent serve --port <n>`
  (default `CLOUD_PORT` or 3000).
* `aria-agent setup` / `aria-agent upgrade [version] [--url <url>]` — CLI
  self-management (`~/.ariacompute/agent-cli.yml`).
* `just migrate` — `sqlx database create` + `sqlx migrate run`
* `just fmt` / `just lint` — formatting and clippy
* Release & publish: `.github/workflows/release.yml` (binary + cdylib assets),
  `publish-cargo.yml` (crates.io: sandbox → core → memo → ariacompute-agent),
  `publish-maven.yml` (Maven Central), `publish-npm.yml`, `publish-pypi.yml`;
  Swift via CocoaPods `bindings/swift/AriaAgent.podspec`.
