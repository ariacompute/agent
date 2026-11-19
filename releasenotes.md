# Release notes

## Rust / Swift / Kotlin SDKs are cloud API clients

The UniFFI surface no longer embeds the runtime: `ariacompute-agent` is now a
thin HTTP client for the beta Agents API, with the same vocabulary as the
JS/Python SDKs. Device binaries carry no loop, sandbox, tool family or model key
(ADR-0020).

### Highlights

- **New surface**: `AriaAgent` (`createSession`, `session`, `run`,
  `runStreamed`), `Session` (`memorize` / `recall` / `itemsJson` / `turnsJson` /
  `titleJson`), `AriaAgentConfig`, `ClientOptions`, `MemoryConfig`, `RunResult`,
  `TokenUsage`, `StreamEvent`, `StreamListener`, `AriaError`.
- **Removed**: the in-process runtime, `create_agent_for_tenant[_with]`,
  `sandbox_provider` / `permission_preset` / `approval_mode`, and the embedded
  context store.
- **Memory** still supports `cloud` (session memory REST), `local` (aria memo
  CLI) and `both`; a missing CLI is an actionable config error.
- **Errors** are typed (`config` / `auth` / `network` / `api` / `unknown`) with
  the HTTP status on `api`.
- **Streaming** delivers every `agent.*` frame to a `StreamListener` and honours
  `maxTurns`; an empty stream is an error, never an empty answer.
- **Cross-SDK parity**: JS and Python enforce the same contracts — an empty stream
  is an `unknown` error, an unknown memory backend is `config`, `local` memory
  rows use the `key: value` convention (so aria memo rows are interchangeable
  between the SDKs and the CLI), and `maxTurns` is worded the same way.
- Bindings regenerated with `just ffi` — Swift/Kotlin callers migrate to the new
  names (README examples updated).

## DeepSeek Harness as the default turn runtime

Turns are now executed by **DeepSeek Harness (dsh)** by default: `dsh` runs as an
external child process (`--profile sdk`) over newline-delimited JSON-RPC on
stdio, so the turn loop, model call and tool execution are dsh's, while this
workspace keeps the Cloud API, memory, tenancy and the frozen streaming envelope.

### Highlights

- **`agent_loop` is a real seam**: `AgentLoop` + `LoopContext`; the previous loop
  moved verbatim into `NativeLoop` and stays available as
  `AGENT_HARNESS=native` (ADR-0019).
- **`aria-agent-dsh`**: stdio transport with an 8 MiB frame cap and drained
  stderr, JSON-RPC client with timeouts and fail-closed server requests, per
  session runtime pool (handshake once, idle TTL, hard capacity), and one pure
  mapping module from dsh notifications to `AgentEvent` / `SessionEvent`.
- **Approvals and questions are bridged** to the installed seams; a missing
  provider denies with the reason instead of allowing. Each session gets its own
  `DSH_HOME` and workspace; the child environment is an allow-list.
- **Seam contract aligned with dsh**: four roles (`core`/`seam`/`bundle`/
  `service`), dsh spellings accepted as aliases (`approval`, `terminals`,
  `spillStore`, …), registration order = precedence, and `dispose` releases
  providers in reverse assembly order. `service` keys reject providers.
- **New tasks**: `just dsh-smoke` (scripted fake runtime, offline) and
  `just dsh-e2e` (one live turn; skips loudly without `ARIA_DSH_E2E=1`, `dsh` and
  a provider key).

### Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGENT_HARNESS` | `dsh` (cloud binary) | `dsh` or `native` |
| `DSH_BIN` | `dsh` on `PATH` | runtime executable |
| `DSH_HOME_ROOT` | `.aria/dsh` | per-tenant/per-session home root |
| `DSH_POOL_MAX` | `8` | live runtimes per tenant |
| `DSH_IDLE_TTL_SEC` | `900` | idle reap window |
| `DEEPSEEK_BASE_URL` / `DEEPSEEK_API_KEY` | — | OpenAI-compatible provider |
| `ARIA_AGENT_HARNESS` | `native` | native SDK opt-in to `dsh` |

## Earlier: capability-seam runtime, no vendored harness

This release replaces the codex-derived single-loop harness with a composable
agent kernel. Every capability is a named seam with a default provider; the
Cloud API (OpenAI beta Agents compatible), the UniFFI surface and the JS/Python
SDKs remain the only public exits.

### Highlights

- **Plugin kernel** (`aria-agent-plugin`): `PluginManifest`, `SeamKey`,
  `SeamRole` and a `Registry` builder. Assembly topo-sorts `requires`, rejects
  duplicate ids, duplicate `core` providers, missing dependencies and cycles,
  then freezes the result. `Agent::with_handlers` resolves every seam once, so
  no registry lookup happens on the hot path (ADR-0014).
- **One tool pipeline**: resolve → permission preset → approval → dispatch →
  post-process. With no approval provider the call is refused as `unavailable`
  and the reason is fed back to the model; oversized results are truncated and
  spilled. `fs` / `shell` / `jobs` share a single sandbox root, and `web_fetch`
  applies an SSRF guard, redirect and body caps, and a content-type allowlist
  (ADR-0016).
- **Session event stream** as the single source of truth: `SessionEvent`
  (`turn_started`, `step`, `item_*`, `text_*`, `approval`, `usage`, `compacted`,
  `title_set`, `turn_*`), `SessionRecorder` with batched flush and retry, and
  `SessionProjection::replay`. Local sessions are JSONL; cloud tables are a
  projection. Usage comes from the provider when available, with a local
  estimate as fallback; context overflow triggers one compaction and one retry
  (ADR-0015).
- **Titles everywhere**: a deterministic first-prompt title (written once) with
  an optional async provider override. `run`, `run_stream` and
  `run_event_stream` all record the same turn vocabulary and flush the title
  before the model call, so a delegated child gets a titled, addressable
  session. `GET /v1/agents/sessions/{id}/title` reads the projection; child ids
  (`<parent>::sub::<id>`) are authorised through their parent tenant.
- **Orchestration** (`aria-agent-orchestrator`): in-process sub-agents with
  namespaced child sessions, a concurrency cap and cached `wait`; agent teams
  as validated task DAGs with dependency-gated claims; scheduled jobs that
  advance `next_run_ms` before running under a single-flight ticker (ADR-0017).
- **Real-socket E2E suite**: the production axum router is bound to an ephemeral
  loopback port and driven by a real HTTP client, covering the beta Agents
  lifecycle, SSE frames, tool calls, tenant isolation, memory and error paths.
  Postgres/pgvector and Docker scenarios skip with a reason when their
  dependency is absent.

### New crates

- `aria-agent-plugin` — manifest, registry, assembly validation, frozen handle.
- `aria-agent-tools` — fs, shell, jobs, terminal (`pty`, off by default),
  skills, web (`web`, off by default), ask_user, plan, todo and memo families;
  memo tools moved out of `aria-agent-core`.
- `aria-agent-orchestrator` — `subagent_*`, `team_*` and `schedule_*` tools.

`aria-agent-core` was split into `agent_loop`, `prompt`, `token_meter`, `seams/*`
and `session/*`; it still does not depend on any store implementation.

### Cloud API & SDK

- New migrations `0006_scheduled_jobs`, `0007_agent_teams`, `0008_session_titles`
  (idempotent, `principal_id`-scoped).
- New routes: `/v1/agent-teams`, `/v1/scheduled-jobs` and
  `GET /v1/agents/sessions/{id}/title`; orchestrator tools are wired per
  principal, and sub-agents use a child-agent factory with delegation depth one.
- SSE adds `agent.turn.approval.requested`, `agent.turn.approval.resolved` and
  `agent.turn.usage`; existing frames and the terminal-event semantics are
  unchanged.
- `SdkAgentConfig` gains `permission_preset` and `approval_mode`; Swift/Kotlin
  bindings were regenerated with `just ffi`. JS and Python decode the new
  frames and expose `result.usage` (`TokenUsage`).

### Breaking changes

- The `codex` submodule, all codex git dependencies, the exact `rama-*` pins,
  the `[patch.crates-io]` section, `CodexSandbox` and
  `SandboxProvider::Codex` are gone (ADR-0018).
- `sandbox_provider = "codex"` is a hard configuration error, with no silent
  fallback; docker is the default and kata/cube/browser remain selectable.
- A deployment without an approval provider now refuses mutations instead of
  implicitly allowing them; unattended writes require `approval_mode = auto`
  plus an explicit write preset.
- The deprecated `Agent::new` / `with_model` / `with_sandbox` constructors remain
  as thin wrappers. The `shell` tool name and its argument shape are unchanged.

### Validation

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` (641 passed), `just e2e` (5 passed; DB/Docker cases skip
without their dependencies), `just ffi`, `just sdk-js-test` (53 passed) and
`just sdk-py-test` (30 passed) all succeed.
