# Release notes

## Governed execution: one egress boundary, audit, metering and credentials

Every outbound request now crosses a single boundary that decides, substitutes and
records (ADR-0020). `AGENT_EGRESS_ENFORCEMENT=network` makes that boundary the
*only* route out: sandboxes run on an `internal` Docker network whose sole
reachable peer is a secretless proxy (ADR-0024).

### Highlights

- **Four new seams** — `egress`, `audit`, `metering` and `credentials`. Each has a
  no-op default that refuses (`DenyAllEgress`, `NoAuditConfigured`,
  `NoMeteringConfigured`, `NoCredentials`) instead of allowing, so missing wiring
  fails closed.
- **`aria-agent-governance`** — `EgressGateway` (decide → substitute
  `${credential}` → audit → meter), `LocalForwardProxy` (HTTP/1.1 absolute-URI
  forwarding plus blind `CONNECT`), `JsonlAuditSink` (UTC day files, retention
  purge, NDJSON export), `MeteringLedger` + `QuotaGuardSandbox` (a refused
  reservation rolls the container back), `GovernanceProjector` (projects tool
  output for the model only; events keep the raw result), `FileCredentialStore`
  (ChaCha20-Poly1305 at rest, rotation deadlines, header stubs) and idle detection
  over five independent signals.
- **`aria-agent-egress-proxy`** — a separate image that holds no secrets and
  writes no disk. It implements the same seams over `/internal/egress/*`, so
  decision, redaction and audit semantics are reused rather than reimplemented.
  Identity is the peer IP, resolved from container labels (`aria.tenant_id` /
  `aria.session_id`) and cached 10s / 4096 peers; an unknown peer is refused, and
  a `principal_id` that does not own that IP is rejected — a compromised proxy
  cannot cross tenants.
- **Fail boot, never downgrade** — in `network` mode the service creates the
  network, starts the proxy, attaches it to the control-plane network and probes
  health before serving. Any failure exits with the next action ("run in a
  container, or use `enforcement=proxy`").
- **Structural coverage** — the wrapper is applied where the runtime is assembled
  (`Agent::with_*`) and to the browser backend's inner Docker sandbox, so `shell`,
  `job_*`, the loop and `browser_*` all land on the internal network. `term_*` is
  unregistered (host PTY, no container network can cover it), `cube` refuses a
  custom network and labels but forwards the proxy environment, and a restored
  checkpoint joins the same network with the authenticated tenant label.
- **Cloud surface** — `/v1/governance/*`: policy revisions (stale writes are
  `409`), audit + NDJSON export, usage, status (enforcement mode + proxy health),
  credential metadata (when a store is configured), and checkpoints with restore.
  Four `principal_id`-scoped tables (`egress_policies`, `audit_events`,
  `usage_events`, `sandbox_checkpoints`) are created with `CREATE TABLE IF NOT
  EXISTS`, so an existing database needs no manual step; credentials stay in the
  encrypted on-disk store.
- **Observability** — the proxy is not a compose service, so `docker network ls`
  shows the internal network and `docker ps` the sidecar; a denial reaches the
  model as `egress is deny-by-default and x is not on the allowlist (policy rN)`
  instead of a timeout.

### Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGENT_EGRESS_MODE` | `deny_all` | `deny_all` \| `allowlist` \| `denylist` |
| `AGENT_EGRESS_ALLOW` / `AGENT_EGRESS_DENY` | *(empty)* | host patterns; `*.` matches sub-domains |
| `AGENT_EGRESS_ENFORCEMENT` | `proxy` | `proxy` \| `network` \| `off` (needs `AGENT_ALLOW_UNENFORCED_EGRESS=1`) |
| `AGENT_EGRESS_NETWORK` / `AGENT_EGRESS_PROXY_URL` | `aria-egress` / `http://aria-egress-proxy:8080` | internal network and the address reachable from a sandbox |
| `EGRESS_PROXY_IMAGE` / `EGRESS_PROXY_TOKEN` | — / generated per boot | sidecar image (required for `network`) and the shared token |
| `CONTROL_PLANE_URL` | *(required for `network`)* | how the proxy reaches this service |
| `AGENT_AUDIT_DIR` / `AGENT_AUDIT_RETENTION_DAYS` | `<workspace>/.aria/audit` / `30` | JSONL sink; `0` keeps forever |
| `AGENT_METERING` / `AGENT_COMPRESSION` | `on` / `on` | usage ledger / model-facing projection |
| `AGENT_CREDENTIALS_DIR` / `AGENT_CREDENTIAL_MASTER_KEY` | *(unset → off)* / required when on | encrypted credential store |
| `AGENT_IDLE_TIMEOUT_SECS` | `900` | `0` disables suspension |

### Breaking changes

- `ExecSpec` gained `network` and `labels`, so out-of-tree struct literals no
  longer compile; build with `ExecSpec::command(..)` and set the new fields.
- An `allowlist` policy with no hosts denies every request at call time (it boots
  fine); `deny_all` is the explicit way to say that.
- `network` enforcement refuses to start without `EGRESS_PROXY_IMAGE`,
  `CONTROL_PLANE_URL` and a running proxy.

### Validation

`cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets
-- -D warnings`, `cargo test --locked --workspace` (941 passed) and
`./scripts/coverage.sh` (99.95% lines, 25127/25140; the remainder is either a
documented `// LCOV_EXCL_LINE` opt-out or a closing-delimiter artifact) all pass.

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
