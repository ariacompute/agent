# Release notes

## dsh runs in a container now

The default turn runtime used to be a **host child process**: `dsh --profile sdk` executed the
model call *and* its own tools on the same filesystem as the control plane, with dsh's
`workspace-write` permission preset as the only boundary — a logical rule, not a filesystem one.
ADR-0027 moves that session into a container, and it is on by default.

### Highlights

- **A session container, not a command container** — the sandbox seam gained `exec_attached`
  (one long-lived `exec` with duplex stdin/stdout), `copy_in` (tar upload) and `has_image`;
  `ExecSpec` gained `mounts`, `hardening` and `resources`. Existing backends keep compiling
  because every new `Sandbox` method defaults to `Unsupported`.
- **Keep-alive + attach (form F2)** — `attach_container` was rejected: Docker's
  `NewlineLogOutputDecoder` assumes a tty, but the harness speaks 8-byte-framed JSON-RPC.
  Instead the container runs `while true; do sleep 3600; done`, the workspace is prepared inside
  it, and only then is `dsh --profile sdk` attached. The old one-hour cap is gone: the container
  lives exactly as long as the session.
- **Named volume, not a bind mount** — the cloud talks to the daemon over
  `/var/run/docker.sock` (DooD), so a bind source would resolve on the *daemon's* host. Sessions
  live on `aria-dsh-<tenant>-<session>`, which is why a resumed session finds its state after an
  idle reap **and** after a container rebuild. `DSH_SANDBOX_MOUNT=bind` exists for deployments
  that do share a filesystem with the daemon.
- **Fail-closed** — `DSH_SANDBOX` defaults to `1`; a missing image (nothing pulls), an
  unpreparable workspace, a refused catalog upload or an unattachable harness aborts the boot or
  destroys the container. There is no silent host fallback, because that would turn "sandboxed"
  into an unchecked claim. `DSH_SANDBOX=0` is the explicit, logged escape hatch.
- **The thinking filter learns about containers** — a container's `127.0.0.1` is the container,
  so `DSH_THINKING_FILTER_BIND` chooses where the proxy listens and `DSH_THINKING_FILTER_URL` is
  the address the harness dials. Enabling the filter while sandboxed without a reachable,
  non-loopback URL fails at boot instead of failing the first turn.
- **A separate budget** — the harness container is not a shell command: it gets its own
  `DSH_SANDBOX_CPUS/MEMORY/PIDS` (default 1 CPU / 1 GiB / 512 pids), joins the governed egress
  network with tenant/session labels, and receives `GATEWAY_*` plus the proxy variables through
  the allow-list — never the host environment.
- **`SandboxShell` owns its timeout** — a command that exceeds its `timeout_ms` is reported as
  `Unavailable`, and the container is destroyed on every path, including the timeout path.
- **A buildable image** — `docker build --target sandbox` produces it (venv dsh symlinked to
  `/usr/local/bin/dsh`, uid 10001, `WORKDIR /aria` so runc never creates a root-owned directory
  inside the session mount). The stage stays first, because the legacy builder would otherwise
  compile the whole workspace for `--target sandbox`, and its pip step defaults to a PyPI mirror
  with resume enabled — the ~100 MB runtime wheel did not finish from pypi.org on a consumer link.

### Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `DSH_SANDBOX` | `1` | Containerise the harness; boot fails if the image or workspace is unusable |
| `DSH_SANDBOX_IMAGE` | `aria-dsh-sandbox:latest` | Built by `just sandbox-image`; never pulled |
| `DSH_SANDBOX_MOUNT` | `volume` | `volume` (`aria-dsh-<tenant>-<session>`) or `bind` (the directory must be writable by `DSH_SANDBOX_USER`) |
| `DSH_SANDBOX_ROOT` / `DSH_SANDBOX_BIN` | `/aria/session` / `/usr/local/bin/dsh` | Container-side mount point and executable |
| `DSH_SANDBOX_VOLUME_PREFIX` | `aria-dsh` | Volume name prefix |
| `DSH_SANDBOX_CPUS` / `DSH_SANDBOX_MEMORY` / `DSH_SANDBOX_PIDS` | `1.0` / `1g` / `512` | Harness container budget |
| `DSH_SANDBOX_USER` / `DSH_SANDBOX_HARDEN` | `10001:10001` / `1` | Non-root user; drop all caps + `no-new-privileges` |
| `DSH_THINKING_FILTER_BIND` / `DSH_THINKING_FILTER_URL` | `127.0.0.1:0` / _(empty)_ | Where the filter listens / the container-reachable address |

### Breaking changes

- `build_host_config(res, runtime, spec: &ExecSpec)` replaced its `network: Option<&str>`
  argument; out-of-tree callers must pass the spec (mounts, hardening and limits are read from
  it).
- `DshConfig` gained `session_root`, `session` and `sandbox`; build it with
  `DshConfig::for_session_like(..)` or `..Default`-style construction.
- `DSH_SANDBOX` defaults to on: a deployment with a `dsh` harness now needs the image locally.
  Set `DSH_SANDBOX=0` to keep the previous host-process behaviour (and its weaker boundary).
- An explicitly empty `DSH_SANDBOX_IMAGE` / `DSH_SANDBOX_BIN` / `DSH_SANDBOX_VOLUME_PREFIX` is
  now an error instead of falling back to the default.

### Residual risks (recorded in ADR-0027, not solved here)

- `EGRESS_MODE=network` removes the default route; whether dsh honours `HTTPS_PROXY` there is
  unverified, and the filter proxy would need a sidecar.
- `cap_drop=ALL` may break dsh's own internal sandbox; `DSH_SANDBOX_HARDEN=0` is the fallback.
- 1 GiB is a first guess: raise `DSH_SANDBOX_MEMORY` if a real turn is OOM-killed.
- Credentials reach the container as environment variables, so `docker inspect` shows them.
- The harness container is not metered by `QuotaGuardSandbox`.

### Validation

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` (1287 passed) and `./scripts/coverage.sh` (every included
line covered: 35073/35073; measured 99.98% with 7 closing-delimiter artifacts,
which carry no statement) all pass. `just sandbox-image` builds the 650 MB image,
the Docker primitives' gated pair (`ARIA_E2E_DOCKER=1`, 2 tests) passes against a
real daemon, and `ARIA_DSH_SANDBOX_E2E=1` completes the containerised
`initialize` handshake.

## Recursive self-improvement: the ledger becomes capability

Every task already leaves a `SessionEvent` ledger. RSI (ADR-0026) folds that
same ledger into diagnosis, turns a diagnosed pathology into a playbook or skill
candidate, scores the candidate by replaying its own history, and publishes it
through a revision-guarded human gate. No second event stream, no background
daemon and no hot swap. The ledger and diagnosis are **on by default**; the one
step that spends money (the real re-run) stays off until asked for.

### Highlights

- **Pure kernel in `aria-agent-core`** — `session::insight` folds records into
  `EfficiencyReport` / `TurnInsight` / `Pathology` with per-finding evidence
  (`seq`, tool, turn). Six named pathologies (repeated tool failure, retry storm,
  approval-denied loop, compaction churn, step-budget exhaustion, repeated
  tool-argument repair) are reported instead of a bare score, and one report is
  capped by `max_records` / `max_turns` with an explicit `truncated` flag.
- **Additive event contract** — `SessionEvent::ItemDone` gained
  `latency_ms: Option<u64>` with `#[serde(default)]`; no variant was added, so
  JSONL written earlier still parses and the frozen SSE envelope is unchanged.
  When a harness does not measure timing, the report simply uses its step/retry
  dimensions instead of failing.
- **`ChainPersistence`** — one append fans out to several sinks in order: the
  first is authoritative (its failure fails the append so `SessionRecorder`
  retries), later sinks are best effort. In the cloud the title projection stays
  first and the RSI ledger is the optional second sink, so a ledger problem never
  turns a completed turn into a failed one.
- **New crate `aria-agent-rsi`** (`publish = false`, depends on
  `aria-agent-core` only) — `RsiLedger` (dedupe per `(principal, session, seq)`,
  bounded window), `DeterministicReflection` (pathology → candidate rule table
  with scope and parameter list, **zero model calls**), replay `validate` (a gain
  below `min_score_gain` discards the candidate), `PlaybookEngine`
  (`current` / `propose` / `promote` / `list_versions`), `FilesystemSkillSink`
  and single-flight `RsiSweep` (a concurrent call reports `busy`).
- **Two new seams** — `rsi` (`ReflectionEngine`) and `playbook`
  (`PlaybookStore`), registered in `SeamKey::ALL`; unregistered keys still fail
  at boot with `UnknownSeam`, and `NoReflection` / `NoPlaybooks` fail closed.
- **Cloud surface** — `GET /v1/rsi/status`, `POST /v1/rsi/sweep`,
  `GET /v1/rsi/reports` + `/{id}`, `GET /v1/rsi/proposals`,
  `POST /v1/rsi/proposals/{id}/promote|discard`,
  `GET /v1/rsi/playbooks/{key}` + `/versions`,
  `POST /v1/rsi/playbooks/{key}/adapt`. With RSI off,
  `POST /v1/rsi/sweep` answers `503` (reads stay available); a stale
  `expected_revision` is `409` (the candidate stays pending); an unadapted
  cross-tenant copy is `422` and creates nothing; non-admin adaptation is `403`;
  unknown ids are `404`. Wire views never carry `principal_id`.
- **Storage** — `migrations/0009_rsi.sql` adds `rsi_reports`, `rsi_proposals`,
  `rsi_playbooks` and `rsi_reruns` (idempotent, `principal_id`-scoped). Every query filters on the
  tenant, promotion runs in one transaction, and
  `UNIQUE (principal_id, domain, key, revision)` makes a racing promotion fail on
  the constraint instead of overwriting a reviewer's work. An in-memory provider
  covers the same semantics offline.
- **Real re-run gate** — `ModelRerunProbe` replays one recorded task through an
  actual model call with the candidate playbook applied. It is tool-free by
  design, so a validation cannot become a second production incident, and every
  attempt writes a receipt to `rsi_reruns` (prompt/completion tokens, model
  round-trips, tool calls = 0, duration, verdict). A timeout, a model error, an
  empty answer or an unwritable receipt is never a pass. It stays off by default
  (`AGENT_RSI_REAL_RERUN`) because it costs model calls.
- **Reuse, not hot swap** — a promoted playbook is a new **version** read at the
  next turn; a promoted skill lands in the shared catalog
  (`AGENT_RSI_SKILLS_DIR`, default `~/.ariacompute/skills`), which is exactly the
  directory `FilesystemSkills` scans and every dsh workspace links to (symlink on
  Unix, copy elsewhere, skipped when empty). A running session's system prompt is
  never rewritten.

### Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGENT_RSI_ENABLED` | `true` | Master switch. `false`: no ledger, no sweep, no timer, and turns keep a single persistence sink |
| `AGENT_RSI_AUTO_PROMOTE` | `false` | Publish validated candidates without asking a human (trusted deployments only) |
| `AGENT_RSI_SWEEP_SECS` | `0` | In-process sweep interval; `0` leaves sweeps to external cron via `POST /v1/rsi/sweep` |
| `AGENT_RSI_SWEEP_THRESHOLD` | `3` | Unhealthy turns accumulated before a sweep is due |
| `AGENT_RSI_DOMAIN` | `default` | Domain the in-process timer sweeps |
| `AGENT_RSI_SKILLS_DIR` | `~/.ariacompute/skills` | Shared catalog: RSI writes it, `FilesystemSkills` reads it, dsh links it |
| `AGENT_RSI_REAL_RERUN` | `false` | Real re-run gate: one tool-free model call per validation, receipted in `rsi_reruns` |
| `AGENT_RSI_RERUN_TIMEOUT_MS` | `600000` | Deadline for one re-run; a timeout is `inconclusive`, never a pass |

### Breaking changes

- Out-of-tree `SessionEvent::ItemDone` struct literals must now set
  `latency_ms` (usually `None`). The wire format stays backward compatible; only
  the literal gains a field.
- `RsiConfig::default().enabled` is now `true`, and the binary reads
  `AGENT_RSI_ENABLED` with a `true` default. Deployments that relied on "unset
  means off" must set `AGENT_RSI_ENABLED=false` to keep the old cost profile.
- `DshConfig` gained a `skills` field; out-of-tree struct literals must add it
  (use `DshConfig::for_session(..)` or `..Default`-style construction).

### Validation

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test --workspace` (1150 passed) and
`./scripts/coverage.sh` (every included line covered: 31650/31650; measured
99.93% with 23 closing-delimiter artifacts, which carry no statement) all pass.

## Unified model gateway configuration

`GATEWAY_BASE_URL` / `GATEWAY_API_KEY` / `GATEWAY_MODEL` now configure model
access for both `AGENT_HARNESS=dsh` and `native`. The separate
`DEEPSEEK_BASE_URL` / `DEEPSEEK_API_KEY` variables are removed, and `GATEWAY_*`
no longer falls back to `OPENAI_*`. For dsh, the gateway values are bridged into
its allow-listed child environment; the model id is taken from `GATEWAY_MODEL`
when set.

## Governed execution: one egress boundary, audit, metering and credentials

Every outbound request now crosses a single boundary that decides, substitutes and
records (ADR-0025). `AGENT_EGRESS_ENFORCEMENT=network` makes that boundary the
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
(ADR-0025).

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
