<div align="center">
<a name="readme-top"></a>

# agent

**A layered agent platform for AI agents**

It is a capability-seam agent runtime that exposes agents as<br/>
(a) a **Rust + Cloud API** speaking the **OpenAI beta Agents** resource surface, (b) **JS / Python<br/>
SDKs** for that HTTP API, and (c) **native SDKs for Swift / Kotlin** — all sharing<br/>
one `recall → runtime → memorize` shell, whose turns run on **DeepSeek Harness**<br/>
by default, with pluggable sandboxes.

[English](./README.md) · **简体中文** · [Docs](./docs/architecture.md)

[![][release-shield]][release-link] [![][language-shield]][language-link] [![][license-shield]][license-link] [![][lastcommit-shield]][lastcommit-link]
<br/>
</div>

<details>
<summary><kbd>Table of contents</kbd></summary>

#### TOC

**Get started**

- [✨ Features](#-features)
- [🏗️ Architecture](#-architecture)
- [🧩 Modules](#-modules)
- [🚀 Quick start](#-quick-start)

**CLI & deploy**

- [🧰 Command line](#-command-line)
- [🛳 Deployment](#-deployment)
- [🔨 Development](#-development)

**Architecture & SDK**

- [🐋 Turn runtime](#-turn-runtime)
- [🌐 Browser sandbox](#-browser-sandbox)
- [🧠 Memory graph & memo tools](#-memory-graph--memo-tools)
- [♻️ Recursive self-improvement](#-recursive-self-improvement)
- [🔌 OpenAI Agents API compatibility](#-openai-agents-api-compatibility)
- [🤝 A2A server](#-a2a-server)

**SDK**

- [📦 SDK examples](#-sdk-examples)

**Trust & governance**

- [🔏 Trust store security](#-trust-store-security)
- [🛡️ Governed execution](#-governed-execution)
- [📚 Documentation](#-documentation)
- [📝 License](#-license)
- [🤝 Engineering conventions](#-engineering-conventions)

</details>

## ✨ Features

`agent` gives every SDK the same three-step shape — define an `Agent`, run it once, then
continue the conversation and stream — while the runtime decides where memory lives and
where tools execute:

- 🐋 **DeepSeek Harness by default** — a turn runs on the external [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) (`dsh --profile sdk`, newline-delimited JSON-RPC over stdio) **inside a `docker build --target sandbox` container** (`DSH_SANDBOX=1`, on by default), so turn/step orchestration, model calls and tool execution are dsh's and the host process is never the boundary; the in-process Rust loop stays as an explicit `native` fallback (`aria-agent-dsh`).
- 🧩 **Agent shell** — `aria-agent-core` keeps `recall → memorize`, prompt assembly, the tool pipeline, session events and the frozen streaming envelope, independent of which runtime drove the turn.
- 🔌 **Capability seams** — model, sandbox, storage, sessions, tools, approvals, sub-agents and scheduling are replaceable seams assembled by a plugin registry that fails closed at boot (`aria-agent-plugin`).
- 🛠️ **One tool pipeline** — resolve → permission preset → approval → dispatch → post-process (truncate + spill), so a call without an approval provider is refused with a reason instead of running (`aria-agent-tools`).
- ☁️ **Cloud API** — `aria-agent-cloud` is an axum service speaking the OpenAI **beta Agents** surface, with Postgres + pgvector holding both metadata *and* context, and SSE streaming.
- 📱 **On-device store** — `aria-agent-memo` (aria memo / SQLite) keeps a local-first context copy that is interchangeable with the cloud backend behind one `ContextStore` trait.
- 🌐 **Five-language SDKs** — all of them (JS / Python / Rust / Swift / Kotlin) are thin clients over the cloud API; nothing is embedded, so a mobile app only needs network access plus an API key.
- 🧭 **Pluggable sandbox** — every tool container is capped by `SANDBOX_CPUS` / `SANDBOX_MEMORY` / `SANDBOX_PIDS_LIMIT` and reaches the daemon through `DOCKER_HOST` / `ARIA_DOCKER_SOCKET`.
- 🛡️ **Governed execution** — every outbound call crosses one boundary that decides, substitutes `${credential}` and records; audit, metering, idle suspension, output projection and checkpoints hang off the same seams, and `AGENT_EGRESS_ENFORCEMENT=network` removes the sandbox's default route instead of trusting `HTTP_PROXY` (`aria-agent-governance`).
- ♻️ **Recursive self-improvement** — the session event ledger doubles as diagnosis input: `aria-agent-rsi` folds it into named pathologies with evidence, proposes a playbook or skill, scores the proposal by replaying history, parks it for a human gate, and can optionally re-run one real, tool-free model turn whose cost lands in `rsi_reruns`. On by default (`AGENT_RSI_ENABLED=false` drops the ledger and the sweep); a cross-tenant copy is still refused until it carries an adaptation record.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🏗️ Architecture

A capability-seam agent runtime: `aria-agent-core` owns the agent shell while every
capability is a replaceable provider, with cloud and native front-ends sharing the
same `recall → runtime → memorize` flow.

### Tech stack

![AGENT tech stack architecture](./arch.png)

### Data flow

A turn flows through recall, prompt assembly, token-pressure control, the model
call, the tool pipeline and terminal events; `SessionEvent` is the single source
of truth for both persistence and SSE streaming.

![AGENT data flow](./dataflow.png)

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🧩 Modules

| Module | What it is |
|--------|------------|
| **aria-agent-memo** | On-device / embedded **context store** (aria memo / SQLite, the same `memories` schema as the `aria-memo` CLI) implementing `aria-agent-core::context`. |
| **aria-agent-sandbox** | Pluggable `Sandbox`: Docker (default) / Kata / Cube / Browser. |
| **aria-agent-browser** | Browser sandbox: six-image `BrowserCatalog` + `BrowserSandbox` (reuses Docker/Kata) + browser tool family. |
| **aria-agent-core** | Agent shell: `recall → runtime → memorize`, seams (`agent_loop` included), session event stream, tool pipeline, prompt assembly. |
| **aria-agent-dsh** | The `agent_loop` seam's DeepSeek Harness provider: stdio JSON-RPC transport, per-session runtime pool, containerised session workspace, approval/question bridge, notification → `AgentEvent` mapping. |
| **aria-agent-plugin** | Plugin registry: manifests, seam keys/roles, dependency topo-sort, cycle + core-uniqueness checks, frozen snapshot. |
| **aria-agent-tools** | Built-in tool families: fs, shell, jobs, terminal (`pty`), skills, web (`web`), ask_user, plan, todo, memo. |
| **aria-agent-governance** | Egress decision + `${credential}` substitution, JSONL audit sink, usage metering, tool-output projection, encrypted credential store, idle detection, quota guard and checkpoints. |
| **aria-agent-rsi** | Recursive self-improvement: ledger accounting, deterministic proposer, replay validation, revision-guarded playbooks, receipted real re-run, shared skill sink and sweep. |
| **aria-agent-egress-proxy** | Secretless sidecar for `network` enforcement: speaks `/internal/egress/*` to the control plane, resolves the tenant from the peer IP, batches audit uploads. |
| **aria-agent-orchestrator** | Sub-agents (in-process fork), agent teams (task DAG), scheduled jobs (single-flight ticker). |
| **ariacompute-agent** | UniFFI `cdylib` (`libaria-agent_ffi`): cloud API client — `AriaAgent` / `Session` / `run` / `runStreamed` / `memorize` / `recall`. |
| **aria-agent-cloud** | axum service: Postgres + pgvector (metadata **and** context), OpenAI beta Agents API. `/v1/agents`, `/v1/agents/sessions`, SSE `/v1/agents/sessions/{id}/events/stream`. |
| **bindings** | `bindings/swift` (SwiftPM) and `bindings/kotlin` (Android). |
| **sdk/js** | npm `@ariacompute/agent` — TypeScript/JavaScript client for the beta Agents API (`Agent`, `run`, `runStreamed`, `tool`, `Session`). |
| **sdk/python** | pip `ariacompute-agent` — Python client for the beta Agents API (`Agent`, `Runner`, `function_tool`, `Session`). |

> **Storage boundary:** the **cloud** stores conversational / long-term context in
> Postgres + pgvector (`context_fragments`, per-principal) — pgvector is required
> and boot fails without it. The SDKs' `local` backend keeps an aria memo (SQLite) copy.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🚀 Quick start

```bash
# 1. build everything
cargo build --workspace

# 2. test
cargo test --workspace

# 3. lint (warnings as errors)
cargo clippy --workspace --all-targets -- -D warnings

# 4. generate Swift/Kotlin bindings (needs the cdylib built)
# = cargo run -p aria-agent-ffigen
just ffi

# 5. run pgvector locally and start the cloud API (cargo run)
[ -f .env ] || cp .env.example .env  # then set GATEWAY_API_KEY in .env
set -a
source ./.env
set +a

# use a non-conflicting host port (55432); localhost:5432 may be a plain Postgres
docker rm -f aria-agent-pgvector >/dev/null 2>&1 || true
docker run -d --name aria-agent-pgvector \
  -e POSTGRES_USER="${POSTGRES_USER:-postgres}" \
  -e POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-postgres}" \
  -e POSTGRES_DB="${POSTGRES_DB:-agent}" \
  -p 55432:5432 \
  pgvector/pgvector:pg16
until docker exec aria-agent-pgvector pg_isready -U "${POSTGRES_USER:-postgres}" -d "${POSTGRES_DB:-agent}" >/dev/null 2>&1; do sleep 1; done

# .env's DATABASE_URL uses the compose host `postgres`; point local cargo at pgvector
export DATABASE_URL="postgres://${POSTGRES_USER:-postgres}:${POSTGRES_PASSWORD:-postgres}@localhost:55432/${POSTGRES_DB:-agent}"
cargo run -p aria-agent-cloud
```

> The pgvector container is recreated on each run; `ensure_schema` creates the
> tables on boot. If 55432 is taken, change both `-p` and `DATABASE_URL`.
>
> `.env.example` ships `DSH_SANDBOX=1`, so the first `cargo run` also expects
> `aria-dsh-sandbox:latest`: run `just sandbox-image` once (nothing pulls it), or
> set `DSH_SANDBOX=0` for a purely local run — the harness then stays a host
> process with dsh's permission preset as the only boundary.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🧰 Command line

The `aria-agent` binary (crate `aria-agent-cloud`) is both the cloud server
(default subcommand `serve`) and a self-updating CLI. CLI settings live in a
small sidecar config at `~/.ariacompute/agent-cli.yml` (override the home dir
with `ARIA_COMPUTE_HOME`).

```bash
# Configure the Releases source interactively (github default, or gitee).
# The choice is written to ~/.ariacompute/agent-cli.yml as `upgrade_url`.
aria-agent setup
# 1) github  -> https://github.com/ariacompute
# 2) gitee   -> https://gitee.com/ariacompute

# Inspect the current CLI config.
aria-agent setup --status

# Remove the CLI config file.
aria-agent setup --clear

# Self-update this binary + libaria-agent_ffi from Releases.
# Reads `upgrade_url` from agent-cli.yml; pass --url to override.
aria-agent upgrade        # latest stable
aria-agent upgrade 0.7.2  # specific version
aria-agent upgrade --url https://github.com/ariacompute

# Start the cloud HTTP server (default subcommand). Reads agent-cli.yml and
# exports ARIA_AGENT_FFI_LIB to ~/.ariacompute/lib.
aria-agent serve
# --port <n>  listen port (default: CLOUD_PORT env or 3000)
aria-agent serve --port 3000
```

> The `serve` subcommand also reads `~/.ariacompute/agent-cli.yml` and points
> `ARIA_AGENT_FFI_LIB` at `~/.ariacompute/lib` (where `upgrade` installs
> `libaria-agent_ffi`) so native SDKs can find the cdylib.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🛳 Deployment

The cloud service ships with a `Dockerfile` and `docker-compose.yml` for a
self-contained deployment (Postgres + the `aria-agent` axum service). All
configuration is provided through a local `.env` file.

### 1. Configure

Copy the template and edit the values:

```bash
cp .env.example .env
```

| Variable | Default | Description |
|----------|---------|-------------|
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `postgres` / `postgres` / `agent` | Postgres (**pgvector**) credentials + database: metadata *and* context memory. |
| `DATABASE_URL` | `postgres://postgres:postgres@postgres:5432/agent` | Connection string. The host `postgres` is the compose service name. |
| `AGENT_HARNESS` | `dsh` | Turn runtime: `dsh` (DeepSeek Harness) or `native` (built-in Rust loop). An unknown value fails at boot. |
| `DSH_BIN` | `/opt/dsh/bin/dsh` | Host harness executable, used only when `DSH_SANDBOX=0`. Sandboxed runs execute `DSH_SANDBOX_BIN` inside the container. Missing binary = actionable error, never a downgrade. |
| `DSH_PROVIDER` | `deepseek-official` | Provider route inside dsh. Aria passes it through unchanged; an unknown route fails in the harness instead of being remapped. |
| `DSH_HOME_ROOT` | `/app/.aria/dsh` | Root for per-tenant/per-session `DSH_HOME` directories (unset resolves `.aria/dsh`). |
| `DSH_POOL_MAX` | `8` | Live harness runtimes kept per tenant. |
| `DSH_IDLE_TTL_SEC` | `900` | Idle window before a pooled runtime is reaped (sessions resume from `DSH_HOME`). |
| `DSH_THINKING_FILTER` | `enable` | `enable` routes dsh through a proxy that strips Anthropic `redacted_thinking` blocks in both directions (the bundled adapter cannot replay them and fails turn two with `UNSUPPORTED_CONTENT`); `thinking` blocks are preserved. `disable` connects dsh straight to the gateway. Any other value fails at boot. |
| `DSH_PERMISSION_MODE` | `workspace-write` | Tool-permission ceiling dsh applies inside the sandbox (`read-only` \| `workspace-write` \| `danger-full-access`). Never set it wider than the sandbox allows. |
| `DSH_SANDBOX` | `1` | Run the harness in a container instead of a host child process (**on by default**, fail-closed). A missing image, an unpreparable workspace or an unattachable harness aborts boot; there is no host fallback. `0` is the explicit, logged escape hatch. |
| `DSH_SANDBOX_IMAGE` | `aria-dsh-sandbox:latest` | Image built by `just sandbox-image` (`docker build --target sandbox`). **Nothing pulls it**: it must exist locally before the cloud starts, or preflight fails. |
| `DSH_SANDBOX_MOUNT` | `volume` | Session workspace backing: `volume` (named `aria-dsh-<tenant>-<session>`, DooD-safe) or `bind` (host path; only when the control plane shares a filesystem with the daemon — the directory must be writable by `DSH_SANDBOX_USER`, e.g. `DSH_SANDBOX_USER="$(id -u):$(id -g)"` locally). |
| `DSH_SANDBOX_ROOT` / `DSH_SANDBOX_BIN` | `/aria/session` / `/usr/local/bin/dsh` | Mount point **inside** the container and the harness binary *inside* it. The host's `DSH_BIN` and `PATH` are never forwarded. |
| `DSH_SANDBOX_VOLUME_PREFIX` | `aria-dsh` | Prefix for the per-session volume name. Volumes deliberately outlive a container (session resume) and are pruned by name. |
| `DSH_SANDBOX_CPUS` / `DSH_SANDBOX_MEMORY` / `DSH_SANDBOX_PIDS` | `1.0` / `1g` / `512` | Container budget for the harness, separate from `SANDBOX_*` — dsh is heavier than a shell command. |
| `DSH_SANDBOX_USER` / `DSH_SANDBOX_HARDEN` | `10001:10001` / `1` | Non-root user inside the image, and drop-all-capabilities + `no-new-privileges`. An empty user keeps the image's own. |
| `DSH_THINKING_FILTER_BIND` / `DSH_THINKING_FILTER_URL` | `127.0.0.1:0` / _(empty)_ | Where the filter listens, and the address the **container** dials. A container's `127.0.0.1` is itself, so a sandboxed deployment with the filter enabled must set both (e.g. `0.0.0.0:8787` and `http://172.17.0.1:8787`) or boot fails. |
| `GATEWAY_BASE_URL` / `GATEWAY_API_KEY` / `GATEWAY_MODEL` | `https://gateway.ariacompute.com` / `your-api-key` (placeholder) / `ariacompute/ariamodel` | Model gateway config shared by **both** `dsh` and `native`. The value is the gateway root; `native` appends `/v1/chat/completions` and `dsh` appends `/anthropic/v1/messages`. No `OPENAI_*` fallback. `dsh` receives the values through its child-process allow-list. Leave empty only for stub/offline runs. |
| `AGENT_CLOUD_API_KEY` | `your-api-key` (placeholder) | Bootstrap **admin** key. When set, it is the Admin principal (present as `Authorization: Bearer <key>` / `ApiKey <key>`); admins mint per-tenant keys via `POST /v1/api-keys`. Requests without a valid key get `401`. The template ships a placeholder — replace it. Empty = open (`default` tenant). |
| `API_KEY_CACHE_TTL_SEC` | `60` | How long a resolved API key stays cached before it is re-checked in Postgres. |
| `AGENT_RSI_ENABLED` / `AGENT_RSI_AUTO_PROMOTE` | `true` / `false` | Recursive self-improvement master switch and the trusted-deployment auto gate. Setting `AGENT_RSI_ENABLED=false` means no ledger, no sweep and a single persistence sink per turn. |
| `AGENT_RSI_SWEEP_SECS` / `AGENT_RSI_SWEEP_THRESHOLD` / `AGENT_RSI_DOMAIN` | `0` / `3` / `default` | In-process sweep interval (`0` = external cron drives `POST /v1/rsi/sweep`), unhealthy turns accumulated before a sweep is due, and the domain the in-process timer sweeps. |
| `AGENT_RSI_SKILLS_DIR` | _(empty)_ → `~/.ariacompute/skills` | The one catalog promoted skills land in, `FilesystemSkills` scans and every dsh workspace links. Empty resolves `$ARIA_COMPUTE_HOME/skills`, else `$HOME/.ariacompute/skills`. |
| `AGENT_RSI_REAL_RERUN` / `AGENT_RSI_RERUN_TIMEOUT_MS` | `false` / `600000` | Second validation gate: replay one recorded task through a real model call under the candidate playbook. Off by default because it costs money; every attempt is written to `rsi_reruns` (tokens, steps, duration, verdict) and the probe is tool-free, so a re-run has no sandbox side effects. A timeout is inconclusive, never a pass. |
| `A2A_DEFAULT_AGENT` | _(empty)_ | Agent behind the bare `/a2a` endpoint and the well-known card. Empty (or an id that does not exist) only means there is no default: those two routes answer `agent_not_found`, while `/v1/agents/{id}/a2a` still works. |
| `A2A_PUBLIC_URL` | _(empty)_ → `http://127.0.0.1:<port>` | Origin the card advertises (`url = {A2A_PUBLIC_URL}/a2a`). A card cannot be derived from a proxied request, so a real deployment must set it; the loopback fallback logs a warning at boot. |
| `A2A_CARDS_FILE` | _(empty)_ | JSON file overriding the card's `description` and `skills` (camelCase). Without it the card says `"{name} (A2A agent)"` and offers one neutral `chat` skill. A malformed file **fails boot**. |
| `RUST_LOG` | `info` | Rust log filter: `error` \| `warn` \| `info` \| `debug` \| `trace`. |
| `CLOUD_PORT` | `3000` | Host port published for **direct** access to the `cloud` service (the container always listens on 3000). |
| `DOCKER_HOST` | `unix:///var/run/docker.sock` | Docker Engine API endpoint for the sandbox (DooD). Override for a remote / DinD daemon. |
| `ARIA_DOCKER_SOCKET` | _(unset)_ | Explicit sandbox socket path; takes precedence over auto-discovery (e.g. `~/.docker/run/docker.sock` on macOS). |
| `DOCKER_GID` | `998` | GID of the host `docker` group that owns `/var/run/docker.sock`. The runtime image recreates this group and adds the non-root `aria` user to it so the socket is reachable without `root`. |
| `SANDBOX_CPUS` | `1.0` | Per-tool sandbox CPU cap (e.g. `1.0`, `0.5`). Applied to Docker & Kata via the Engine API. |
| `SANDBOX_MEMORY` | `512m` | Per-tool sandbox memory cap, human-readable units (`512m`, `1g`, …). Applied to Docker & Kata; best-effort for Cube. |
| `SANDBOX_PIDS_LIMIT` | `256` | Max processes inside the sandbox (pids cgroup). Applied to Docker & Kata; best-effort for Cube. |
| `NGINX_PORT` | `80` | Host port published by the `nginx` reverse proxy (forwards to `cloud:3000`). |
| `ARIACOMPUTE_BROWSER_IMAGE_<KIND>` | _(unset)_ | Per-browser image override for the browser sandbox; `<KIND>` ∈ `SERVO` / `OBSCURA` / `CHROMIUM` / `GOSUB` / `CAMOUFOX` / `LIGHTPANDA` (e.g. `ARIACOMPUTE_BROWSER_IMAGE_CHROMIUM=my/playwright:latest`). Falls back to the built-in default. |

> `.env` is git-ignored; `.env.example` is the committed template. Compose
> auto-loads `.env` for variable substitution and the `cloud` service also reads
> it via `env_file`, so the container process receives every variable directly.

> **Client key consistency.** Every client (e.g. the cockpit Android app built
> with `-PagentCloudApiKey=...`) sends `AGENT_CLOUD_API_KEY` (or a key minted via
> `POST /v1/api-keys`) verbatim as `Authorization: Bearer <key>`. It must be a key
> **valid on this server**, or the client must send **no key** when this is empty
> (open mode). A key the server does not recognize (e.g. a placeholder like
> `ak_live_x`) gets `401`. Keep this server key and every client's key
> **consistent (same valid key) or both empty.**

### 2. Build

The `cloud` service uses the `aria-agent-cloud` image. You can build it either
manually or let compose build it for you — both paths produce the same image.

**Method A — manual build, then `up` (reuses the pre-built image):**

```bash
docker build \
  --network=host \
  --build-arg BUILD_PROXY=http://127.0.0.1:7897 \
  --build-arg DOCKER_GID=$(stat -c '%g' /var/run/docker.sock) \
  -f Dockerfile -t aria-agent-cloud .
docker compose up
```

**Method B — one-shot compose build + run:**

```bash
export BUILD_PROXY=http://127.0.0.1:7897  # optional; unset = direct internet
export DOCKER_GID=$(stat -c '%g' /var/run/docker.sock)
docker compose up --build
```

Build-time args (all optional):

| Arg | Default | Description |
|-----|---------|-------------|
| `BUILD_PROXY` | _(empty)_ | HTTP(S) proxy for `apt`/`cargo`/`git` **during the build**. Empty ⇒ direct internet. Use `--network=host` (or the compose `build.network: host`) so the build container's `127.0.0.1` reaches a host-side proxy (e.g. Clash on `127.0.0.1:7897`). |
| `DOCKER_GID` | `998` | GID of the host `docker` group owning `/var/run/docker.sock`; the non-root `aria` user joins this group so the mounted socket is reachable. |
| `APT_MIRROR` | `mirrors.aliyun.com` | apt archive mirror *host* (e.g. `mirrors.tuna.tsinghua.edu.cn`, `mirrors.ustc.edu.cn`). |

> `network: host` for the build is a **Linux** requirement to reach a
> `127.0.0.1` proxy. On Docker Desktop (Mac/Windows) build host networking is
> unsupported — listen the proxy on `0.0.0.0`, or drop `network: host` and pass
> the proxy as a routable host address instead.

### 3. Run

```bash
docker compose up --build
```

#### Smoke-test the API

Once the service is up, run the one-shot HTTP smoke test (no build / no
start / no stop — it only probes the running instance). It loads the **same**
`.env` that `docker compose up` used (or `.env.example` as a fallback) for
`AGENT_CLOUD_API_KEY`, but `BASE_URL` is supplied on the command line or as an
environment variable:

```bash
bash scripts/test-api.sh --base-url http://localhost:3000
# or:
BASE_URL=http://localhost:3000 bash scripts/test-api.sh
```

The script asserts status codes + key JSON fields across every route (agents,
sessions, memory, relations, subagents, api-keys, the model-backed `/events`
and `/events/stream` endpoints, and the recognised-but-unimplemented `501`
stubs), prints
`TOTAL / PASS / FAIL / SKIP`, and exits with the failure count. The single-agent
subagent-id route is asserted as `404`, not skipped. `/events` and
`/events/stream` run by default (a configured model gateway is required);
`--events-test` / `EVENTS_TEST` are accepted as no-op aliases for older
invocations.

The cloud service waits for a healthy Postgres before starting; the schema
(`agents` / `runs`) is created automatically on boot (`ensure_schema`), so no
init scripts are needed. The API is then reachable through **either** endpoint:

- **Direct:** `http://localhost:3000` (or `http://localhost:${CLOUD_PORT}`) — the
  `cloud` service port, published for debugging / when nginx is not needed.
- **Via nginx:** `http://localhost:80` (or `http://localhost:${NGINX_PORT}`) — the
  `nginx` reverse proxy, which forwards to `cloud:3000` (recommended entrypoint;
  adds `X-Forwarded-*` headers and streams SSE without buffering).

`GET /healthz` is the liveness probe: it is served by the `cloud` service itself
(unauthenticated, no API key needed) and `nginx` forwards `/healthz` upstream, so
the answer reflects real backend health. The compose health check for `cloud`
issues `curl -fsS http://localhost:3000/healthz`; `nginx` only starts once
`cloud` reports healthy.

### 4. Context storage (PostgreSQL + pgvector)

Conversational / long-term context lives in Postgres (`context_fragments`,
`vector(256)` with an HNSW cosine index), sharded per `principal_id`. The compose
Postgres image is `pgvector/pgvector:pg16`; the service runs
`CREATE EXTENSION IF NOT EXISTS vector` at boot and **refuses to start** if the
extension is unavailable (no silent keyword-only fallback).

The SDKs' `local` memory backend keeps an aria memo (SQLite) copy on the device
(through the `aria-memo` CLI) and is not affected by this requirement.

### 5. Sandbox via the Docker socket

When an agent invokes a tool that needs isolation, the cloud service spawns a
short-lived container through the Docker **Engine API** (the default
`docker` sandbox provider). This mirrors the playground's model: the `cloud`
container mounts the **host** Docker socket (`/var/run/docker.sock`, DooD) and
reaches the daemon directly — **no `docker` CLI is installed in the image**.

The endpoint is controlled by `DOCKER_HOST` (default `unix:///var/run/docker.sock`),
so you can instead point it at a remote daemon or a Docker-in-Docker sidecar
(e.g. `tcp://dind:2375`) without code changes. Tool containers run on the host
daemon and are named `aria-sandbox-<uuid>`; they are force-removed when the
session ends.

The connection is established **lazily** on the first sandbox call, so a missing
daemon is a tool-execution error rather than a startup panic. When
`DOCKER_HOST` / `ARIA_DOCKER_SOCKET` are unset, the well-known socket locations
are probed in order — including `~/.docker/run/docker.sock`, which is where
**Docker Desktop on macOS** exposes the daemon (`/var/run/docker.sock` only
exists there if Docker Desktop's "default socket" option is enabled), plus
Colima (`~/.colima/default/docker.sock`) and rootless Podman paths.

The `cloud` container runs as the non-root `aria` user. To reach the mounted
socket it joins a `docker` group whose GID is `DOCKER_GID` (default `998`) and
must match the host group that owns `/var/run/docker.sock`
(`stat -c '%g' /var/run/docker.sock`, often 998 or 999). If they differ, rebuild
with `docker compose build --build-arg DOCKER_GID=<gid>`, or fall back to
`user: root` in the compose file. To disable sandboxing entirely, comment out the
`/var/run/docker.sock` volume mount (the `DOCKER_GID` build arg is then unused).

> Mounting the host socket grants the cloud container root-equivalent control of
> the host Docker daemon. Only do this for a trusted, single-tenant deployment.
> For stronger isolation, set `DOCKER_HOST` to a separate daemon (DinD / remote).

#### Auto-detecting `DOCKER_GID`

The `DOCKER_GID` in `.env` must match the host group that owns the socket.
Read it once and write it straight into `.env`:

```bash
DOCKER_GID=$(stat -c '%g' /var/run/docker.sock)
sed -i "s/^DOCKER_GID=.*/DOCKER_GID=$DOCKER_GID/" .env
echo "DOCKER_GID set to $DOCKER_GID"
```

Then rebuild so the runtime image picks up the new group:
`docker compose build --build-arg DOCKER_GID=$DOCKER_GID`.

#### Sandbox resource limits

Every tool container is capped by `SANDBOX_CPUS`, `SANDBOX_MEMORY`, and
`SANDBOX_PIDS_LIMIT` (see the table above). For the **Docker** and **Kata**
providers these are passed straight to the Engine API (`HostConfig`:
`NanoCpus`, `Memory`, `PidsLimit`); for **Kata** the container additionally
runs under the `kata` runtime. The **Cube** provider maps them to best-effort
`cube` CLI flags (`--cpus` / `--memory`) and may need adjustment for your
deployed Cube runtime. Invalid values fall back to the defaults and log a
warning, so the service always boots.

### 6. Volumes

| Volume | Backed by | Purpose |
|--------|-----------|---------|
| `pgdata` | Postgres (pgvector) | `agents` / `runs` metadata **and** `context_fragments` (context memory). |

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🔨 Development

| Target | Effect |
|---|---|
| `cargo build --workspace` | build all crates |
| `cargo test --workspace` | run the workspace test suite |
| `just e2e` | run the real-socket HTTP E2E suite (optional DB/Docker cases skip when unavailable) |
| `cargo clippy --workspace --all-targets -- -D warnings` | lint with warnings as errors |
| `just ffi` | regenerate the Swift / Kotlin bindings (needs the cdylib built) |
| `cargo run -p aria-agent-cloud` | run the cloud API (needs Postgres + OpenAI key) |
| `aria-agent serve` | start the cloud HTTP server (default subcommand) |
| `aria-agent setup` / `upgrade` | configure Releases source / self-update the binary + cdylib |

### E2E tests

All E2E-oriented `just` recipes:

| Command | Effect |
|---|---|
| `just e2e` | Run the real-socket HTTP E2E suite (`cargo test -p agent-tests --test e2e_http`); optional DB/Docker cases skip when unavailable. |
| `just dsh-smoke` | Spawn a scripted fake `dsh` and verify the JSON-RPC handshake + turn framing (`cargo test -p aria-agent-dsh --test stdio -- --nocapture`); offline, no credentials. |
| `just dsh-e2e` | Run one live turn against a real `dsh` (`ARIA_DSH_E2E=1 cargo test -p aria-agent-dsh --test e2e_live -- --nocapture`); needs a local `dsh` plus `GATEWAY_BASE_URL` / `GATEWAY_API_KEY` (and optionally `GATEWAY_MODEL`). The recipe only sets `ARIA_DSH_E2E=1`; `just` does not load `.env` by default, so use one of the two loading forms below. |
| `just sandbox-image` | Build the harness image (`docker build --target sandbox -t aria-dsh-sandbox:latest .`). Required before a sandboxed boot; nothing pulls it. |
| `just dsh-sandbox-e2e` | Complete the `initialize` handshake in a real container (`ARIA_DSH_SANDBOX_E2E=1 cargo test -p aria-agent-dsh --test sandbox_live -- --nocapture`); needs the image above and `GATEWAY_BASE_URL` (`GATEWAY_API_KEY` is optional — `initialize` makes no model call). Skips loudly when either is absent. |

**Local `dsh` install.** The image installs `dsh` from `deepseek-harness-sdk`
(which pulls `deepseek-harness-runtime-bin`) into `/opt/dsh/bin/dsh`. Locally:

```bash
cd <repo-root>
python3 -m venv .venv-dsh
.venv-dsh/bin/pip install --upgrade pip
.venv-dsh/bin/pip install deepseek-harness-sdk
test -x .venv-dsh/bin/dsh && echo "dsh ready"
```

`.env` ships with `DSH_BIN=/opt/dsh/bin/dsh` (the image path). Override it with
the venv binary — inline or by editing `.env` — then load `.env` into the test
process with one of these two forms:

```bash
# shell source: put the DSH_BIN override after sourcing
set -a; . ./.env; set +a; DSH_BIN="$PWD/.venv-dsh/bin/dsh" just dsh-e2e

# or let just load .env: an explicit shell DSH_BIN still wins
DSH_BIN="$PWD/.venv-dsh/bin/dsh" just --dotenv-path .env dsh-e2e
```

`set -a` exports every assignment in `.env` so `cargo test` sees
`GATEWAY_API_KEY`; `--dotenv-path` makes `just` load the same file into the
recipe environment. Without either form the test prints
`skip: GATEWAY_API_KEY is not set` and returns `ok` — that is a skip, not a pass.

`just e2e` binds the production axum router to an ephemeral loopback port and drives the beta Agents API with a real HTTP client. It covers the agent/session/turn lifecycle, SSE frames (including the terminal `agent.turn.failed` frame), tool calls, tenant isolation, memory, orchestration, the subagent read surface, the recognised-but-unimplemented `501` resources, the RSI read surface, the A2A surface (public discovery cards, JSON-RPC send/get/cancel with push config, scope and tenant gates, SSE streaming and replay, and a live cancel driven over two sockets) and error paths. Two optional scenarios skip with an explicit reason rather than faking a pass:

- `DATABASE_URL` — exercises the Postgres + pgvector stores through two independently built routers; skipped when unset or unreachable.
- `ARIA_E2E_DOCKER=1` — executes a `shell` tool in a real container via the streaming route; requires a reachable daemon and a locally cached `alpine:latest` image, otherwise skipped.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🐋 Turn runtime

The turn loop is the `agent_loop` seam. Its default provider is **DeepSeek
Harness**: one `dsh` child process per session, spoken to over JSON-RPC, with its
own `DSH_HOME` and workspace, an allow-list environment, and a pooled runtime that
reaps when idle. dsh executes the model call *and* its own tools, so under
`harness = "dsh"` the aria sandbox/tool pipeline is idle; aria still owns recall,
memory writes, session events/titles, approval + question bridging and the wire
envelope. `AGENT_HARNESS=native` selects the built-in Rust loop instead;
an unknown value — or `dsh` without an installed runtime — is a hard error, never
a silent fallback.

By default (`DSH_THINKING_FILTER=enable`) dsh reaches the gateway through a local
filter proxy (loopback unless `DSH_THINKING_FILTER_BIND` says otherwise, which a
containerised harness requires): the bundled `deepseek-official` adapter accepts an
Anthropic `redacted_thinking` block in the first turn, persists it, then throws
`UNSUPPORTED_CONTENT` when replaying history. The proxy strips that block from
request bodies and from JSON/SSE responses (renumbering content-block indices),
so dsh never sees it. `thinking` blocks are deliberately **kept** — DeepSeek's
Anthropic-compatible layer requires them to be passed back in thinking mode, and
stripping them would turn that error into a `400` — which makes thinking
display-only and never replayed, the same trade-off OpenCode makes. Set
`DSH_THINKING_FILTER=disable` only for a dsh build whose adapter handles redacted
thinking.

**Placement.** A sandboxed harness (`DSH_SANDBOX=1`) is a container, not a child
process: `aria-agent-dsh` spawns the image with a keep-alive command, prepares
`DSH_SANDBOX_ROOT` inside it, uploads the skill catalog as a tar and then
attaches `dsh --profile sdk` over one long-lived `exec` (newline-delimited
JSON-RPC, duplex stdio). The workspace lives on a **named volume** (not a host
bind: the cloud talks to the daemon over `/var/run/docker.sock`, so a bind source
would resolve on the daemon's host), which is why a session survives both an idle
reap and a container rebuild. The container joins the governed egress network
with the same tenant/session labels as every other sandbox, gets its budget from
`DSH_SANDBOX_CPUS/MEMORY/PIDS`, and receives `GATEWAY_*` plus the proxy variables
through an allow-list — never the host environment.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🌐 Browser sandbox

`aria-agent-browser` embeds a real, sandboxed browser into the agent runtime. It
reuses the same `Sandbox` trait and Docker/Kata container path as command
execution, so every browser container is capped by `SANDBOX_CPUS`,
`SANDBOX_MEMORY`, and `SANDBOX_PIDS_LIMIT` and reaches the daemon through
`DOCKER_HOST` / `ARIA_DOCKER_SOCKET` exactly like a shell tool.

A `BrowserCatalog` registers **six** browser images, each launched as its own
sandbox container:

| Kind | Engine | Notes |
|------|--------|-------|
| `chromium` | Playwright + Chromium | Default. Full automation: navigate / extract / click / fill / screenshot / evaluate, plus `playwright-captcha` for captcha handling. |
| `camoufox` | Camoufox (Firefox-based) | Anti-detect / anti-bot profile; Playwright-compatible. |
| `lightpanda` | Lightpanda | AI-native headless browser; lightweight extraction. |
| `obscura` | Chromium stealth layer | Fingerprint / bot-mitigation layer over Chromium (`obscura`). |
| `servo` | Servo (Rust) | Native engine; navigate + extract only, other ops degrade to `Unsupported`. |
| `gosub` | Gosub engine (Rust) | Native engine; navigate + extract only, other ops degrade to `Unsupported`. |

Enable it with `sandbox_provider = "browser"` (default kind `chromium`). Choose
a different engine either at startup or **at runtime** — the `browser_use` tool
tears down the current container and re-pulls the selected image, rolling back
safely if the image is unavailable (no panic, no dropped connection):

```json
{ "name": "browser_use", "arguments": { "engine": "camoufox" } }
```

Browser capabilities are exposed as a tool family routed through a `ToolHandler`
before the shell fallback: `browser_navigate`, `browser_extract`, `browser_click`,
`browser_fill`, `browser_screenshot`, `browser_evaluate`, `browser_solve_captcha`,
and `browser_use` (switch). Example tool calls:

```json
{ "name": "browser_navigate", "arguments": { "url": "https://example.com" } }
{ "name": "browser_extract",   "arguments": { "mode": "markdown" } }
```

Image tags are overridable per engine via `ARIACOMPUTE_BROWSER_IMAGE_<KIND>`
(e.g. `ARIACOMPUTE_BROWSER_IMAGE_CHROMIUM`), falling back to the built-in
defaults.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🧠 Memory graph & memo tools

The agent doesn't just read/write the `aria-agent-memo` store — it **actively
manages its own memory** during a run:

* **Memory graph.** On every turn the run loop supplements vector `recall` with a
  bounded graph `expand` (seeded from the recalled fragments) and links the turn's
  user↔assistant fragments with a `Temporal` relation, so later turns retrieve
  context along the conversation graph, not just by embedding similarity.
* **Auto-compact.** When a session's fragment count exceeds the configured
  threshold (default `32` in the SDK) the run loop `compact`s it into a single
  note (idempotent — repeated compactions reuse the same note id, so history stays
  bounded).
* **Memo tool family.** The model can call four long-term-memory tools, routed
  through a `ToolHandler` before the shell fallback:

  | Tool | Purpose |
  |------|---------|
  | `memo_store` | Persist a keyed long-term memory (e.g. a user preference). |
  | `memo_get` | Read a long-term memory back by key. |
  | `memo_search` | Semantic (vector) search over the session's memory. |
  | `memo_related` | Inspect the memory graph: list the `from`/`to`/`kind`/`score` edges. |

Both the memory graph and auto-compact are gated by `AgentConfig::memory_graph`
(off by default in `aria-agent-core` so its unit tests stay hermetic; the SDK
enables it). `aria-agent-core` depends only on the `ContextStore` trait — never on
`aria-agent-memo` — so the local (aria memo / SQLite) and cloud (pgvector)
backends are interchangeable.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## ♻️ Recursive self-improvement

Every task already leaves a `SessionEvent` ledger. RSI is on by default and folds
that ledger into diagnosis, so a pathology one session paid for becomes a
versioned asset the next one can reuse — with no second event stream, no
background daemon and no hot swap. Only the optional real re-run is off:

```
TurnCompleted / TurnFailed
  → ledger    (ChainPersistence: audit sink first, ledger second)
  → sweep     (POST /v1/rsi/sweep, or AGENT_RSI_SWEEP_SECS > 0)
  → insight   → EfficiencyReport + pathology + evidence (seq / tool / turn)
  → propose   → deterministic rule table, scoped to tenant + domain
  → replay    → below min_score_gain = discarded, nothing published
  → rerun     → optional tool-free model call, receipted in rsi_reruns
  → pending   → human promote (revision-guarded) → new playbook version
                                       └→ skill in ~/.ariacompute/skills (linked into dsh)
```

`EfficiencyReport` names the pathology instead of only scoring the turn —
repeated tool failure, retry storm, approval-denied loop, compaction churn,
step-budget exhaustion, repeated tool-argument repair — and every finding keeps
the event positions it was derived from.

| Endpoint | What it does |
|---|---|
| `GET /v1/rsi/status` | Feature flag, gate in force, buffered records and unhealthy sessions |
| `POST /v1/rsi/sweep` | Run a sweep now (the external-cron trigger); one session or the whole tenant |
| `GET /v1/rsi/reports` | Diagnoses and their evidence, newest first |
| `GET /v1/rsi/reports/{id}` | One diagnosis |
| `GET /v1/rsi/proposals` | Pending candidates for the caller's scope |
| `POST /v1/rsi/proposals/{id}/promote` | The human gate; the body carries `expected_revision` |
| `POST /v1/rsi/proposals/{id}/discard` | Drop a candidate without publishing it |
| `GET /v1/rsi/playbooks/{key}` | Current playbook for the domain |
| `GET /v1/rsi/playbooks/{key}/versions` | Revision history for audit |
| `POST /v1/rsi/playbooks/{key}/adapt` | Propose an adapted cross-tenant copy |

Rules the surface enforces:

- **Off means off** — with `AGENT_RSI_ENABLED=false` there is no ledger, no sweep
  and no timer, `POST /v1/rsi/sweep` answers `503`, and a turn keeps its single
  persistence sink.
- **Nothing publishes implicitly** — a `promote` with a stale `expected_revision`
  is `409` and the candidate stays pending; an unanswered gate is a refusal,
  never an implicit allow.
- **Adapt before reuse** — a cross-tenant copy without a complete adaptation
  record (source, target, rewritten parameters, rationale) is `422` and creates
  nothing.
- **Every paid run leaves a receipt** — the optional real re-run is one tool-free model
  call whose tokens, steps and duration are recorded in `rsi_reruns`; if the
  receipt cannot be written, the run fails closed and nothing is promoted.
- **No hot swap** — promotion produces a new version read at the next turn; a
  running session's system prompt is never rewritten.

`AGENT_RSI_AUTO_PROMOTE` exists for trusted deployments only.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🔌 OpenAI Agents API compatibility

`POST /v1/agents/sessions/{id}/events/stream` speaks the **OpenAI beta Agents
streaming protocol**, so an OpenAI client can consume it unchanged. Every SSE
`data:` frame is an `agent.*` event with `event_id`, `session_id`, `turn_id` and a
monotonic `sequence_number`:

| Event | Payload |
|---|---|
| `agent.turn.created` | `turn{id,status,session_id,agent_id}` |
| `agent.turn.in_progress` | phase boundary; extra `phase` (`recall` / `harness` / `step` / `model` / `tool_exec` / `loop_guard`) + `label` |
| `agent.turn.item.added` | `item` — `message` (assistant, `role`) or `function_call` (`name`, `arguments`) |
| `agent.turn.item.done` | completed `item`; a `function_call` also carries the executed `result` |
| `agent.turn.output_text.delta` | `item_id`, `delta` |
| `agent.turn.output_text.done` | `item_id`, `text` |
| `agent.turn.approval.requested` | `approval_id`, `tool`, `item_id` |
| `agent.turn.approval.resolved` | `approval_id`, `tool`, `decision` (`allow` / `deny` / `unavailable`), `reason` |
| `agent.turn.usage` | `usage{input_tokens, output_tokens, total_tokens}`, `model` |
| `agent.turn.completed` | terminal: `turn{status:"completed", output}` |
| `agent.turn.failed` | terminal: `turn{status:"failed"}`, `error` |

`agent.turn.completed` (or `agent.turn.failed`) is the terminal event: when a
client sees it the run is finished — there is no trailing `data: [DONE]`
sentinel.

Extra fields our orchestration needs but the beta schema does not model (the
agentic `phase`, the tool `result`) are carried **inside** these frames, so
strict OpenAI clients simply ignore them. No private `aria.*` frames are emitted.

The supported way to consume this protocol is one of the SDKs above — they decode
these frames for you (`result.finalOutput` / `result.final_output`):

```python
from ariacompute_agent import Agent, Runner

agent = Agent(name="Agent Demo")
streamed = await Runner.run_streamed(agent, "tell me a joke")
async for event in streamed.events:
    if event["type"] == "agent.turn.output_text.delta":
        print(event["delta"], end="", flush=True)
    elif event["type"] == "agent.turn.approval.resolved":
        print(f"\n[tool {event['tool']}: {event['decision']}]")
    elif event["type"] == "agent.turn.usage":
        print(f"\n[tokens: {event['usage']['total_tokens']}]")
```

### Default agent

`ensure_schema` seeds a default agent — `id: agent-demo`,
`name: Agent Demo` — so a fresh deployment (and the Aria Playground) has
something to run out of the box. Deployments seeded with the previous default
(`playground-demo`) are renamed in place on the next boot. List the agents
visible to your key with `GET /v1/agents` (admins see every agent, tenants
only their own). An SDK that references an unknown agent **name** creates that
agent on first session creation; an explicit unknown `agent_id` still returns
404 instead of silently minting a new one.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🤝 A2A server

The same process is also a standard **A2A remote agent**: an orchestrator
discovers it from a public Agent Card and drives it over JSON-RPC. This is a
second projection — the beta-Agents routes, the SSE envelope and the SDKs are
untouched, and no new permission code exists (`write` for `message/*`, `cancel`,
`resume` and push `set`; `read` for `get`, push `get` and the replay stream).

| Endpoint | Auth | What it does |
|---|---|---|
| `GET /.well-known/agent-card.json` | public | Card of `A2A_DEFAULT_AGENT` (aliases: `/.well-known/agent.json`, `/a2a/agent-card`) |
| `GET /v1/agents/{id}/.well-known/agent-card.json` | public | That agent's card (alias: `.../agent-card.json`) |
| `POST /a2a` | Bearer + `write` | JSON-RPC for the default agent |
| `POST /v1/agents/{id}/a2a` | Bearer + `write` | JSON-RPC for one agent, with visibility check |
| `GET /a2a/stream?task=&since=` | Bearer + `read` | Replay the coarse event log, then follow the live task |

Methods: `message/send`, `message/stream`, `tasks/get`, `tasks/cancel`,
`tasks/resume`, `tasks/pushNotification/set|get`, plus the v0.1 aliases
`tasks/send` and `tasks/sendSubscribe`.

- **`contextId` is a session.** It maps to `agent_sessions.id`, so a context
  reuses that dsh session's container and volume, and the conversation also
  appears in the console as an ordinary turn. One live task per context; a second
  is `-32005`.
- **`tasks/cancel` destroys the container.** The state is CASed first, then the
  parked decision is released, the run is aborted and `DshPool::evict` shuts the
  session's runtime down (the sandbox transport destroys the container). The
  volume survives, so the next prompt in that context resumes dsh's history.
- **HITL parks the task.** An approval or question moves it to `input-required`
  and publishes a `final: true` frame; `tasks/resume` answers it (`metadata.decision`,
  then `metadata.answer`, then message text) and returns it to `working`. An
  answer that does not fit the request is `-32602` and the task **stays** parked —
  never an implicit denial. Both harness paths are wired: native injects the same
  object as `ApprovalBroker`/`UserQuestions`; dsh reaches it through the
  `sessionId` that `SessionTag` injects and the `RoutingBridge`.
- **No sentinel.** Frames carry both `kind` and the legacy `event`; the
  `final: true` frame ends the stream. Token deltas are broadcast only, so a
  reconnect gets status, tool spans and the final artifact — `tasks/get` has the
  full answer.
- **Errors travel inside HTTP 200** (a non-2xx would be read as
  `resource_unreachable` by agenthub): `400` only for a broken envelope, `401`
  auth, `403` `insufficient_scope`, card misses `404`. Codes `-32001`…`-32008`
  follow the A2A convention.
- **The card is public and honest.** It carries the agent name and the
  `A2A_CARDS_FILE` description/skills, never `instructions`, credentials or
  sandbox names — and it does not advertise aria's native tool families, because
  the default dsh harness runs its own tools inside the container.

```bash
curl -s localhost:3000/.well-known/agent-card.json | jq '{name,url,protocolVersion,skills}'
curl -s -X POST localhost:3000/a2a -H 'Authorization: Bearer dev' \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{"kind":"message","role":"user","messageId":"m1","parts":[{"kind":"text","text":"hello"}]}}}'
```

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 📦 SDK examples

Every example below uses an **official SDK** — none of them hand-roll HTTP. All
SDKs expose the same shape: define an `Agent`, then run it.

| Language | Install | Package / module |
|---|---|---|
| Python | `pip install ariacompute-agent` | `ariacompute_agent` (cloud API client) |
| TypeScript / JavaScript | `npm i @ariacompute/agent` | `@ariacompute/agent` (cloud API client) |
| Rust | `cargo add ariacompute-agent` | `aria_agent_ffi` (cloud API client) |
| Swift | SwiftPM / CocoaPods `AriaComputeAgent` | `bindings/swift` (cloud API client) |
| Kotlin | `com.ariacompute:agent` | `bindings/kotlin` (cloud API client) |

Every SDK is a thin client over the beta Agents REST API the service exposes —
see [OpenAI Agents API compatibility](#-openai-agents-api-compatibility) for the
wire contract. Rust / Swift / Kotlin use the same HTTP surface through UniFFI, so
there is no embedded runtime, no sandbox and no provider key on the device; the
only local artefact is the optional aria memo copy for `backend = "local"`.

Cloud SDKs need a running service (see Quick start). Configure
`ARIA_AGENT_BASE_URL` (default `http://localhost:3000`) and, when
`AGENT_CLOUD_API_KEY` is set, `ARIA_AGENT_API_KEY`.

Every example follows the same three steps:

1. **one-shot run** — `run(agent, input)` / `Runner.run(agent, input)`,
2. **continue the same conversation** by reusing the session (after writing a
   long-term memory with `memorize`),
3. **streaming run** — `runStreamed` / `Runner.run_streamed`.

Long-term memory lives in the session's context store: `memorize(key, value)`
persists a fact (as a `long_term` context fragment) that later turns of the same
session recall semantically — even after a restart. Every SDK shares the same
three backends (below): `cloud` is the service's Postgres + pgvector store, and
`local` is the aria memo SQLite store reached through the `aria-memo` CLI. No SDK
embeds a store of its own — that includes Rust / Swift / Kotlin.

### Memory backends: cloud / local / both

Every SDK can choose where the memory context lives:

| Backend | Storage | Notes |
|---|---|---|
| `cloud` | agent-cloud, Postgres + pgvector | shared across devices; needs the service running |
| `local` | **aria memo** (SQLite, `memo.db`) | on-device, works offline; inspectable with `aria-memo list --json` |
| `both` | cloud **and** local | writes to both; reads merged + deduped (cloud copy wins) |

Configure it on the agent / session constructor and override per call:

```python
agent = Agent(name="History tutor", memory_backend="both", memo_db="~/.ariacompute/memo.db")
session.memorize("user_name", "Ada")          # -> both backends
session.recall("user_name", backend="local")  # -> local copy only
```

```typescript
const agent = new Agent({
  name: "History tutor",
  memory: { backend: "both", memoDb: "~/.ariacompute/memo.db" },
});
await session.memorize("user_name", "Ada");               // -> both backends
await session.recall("user_name", { backend: "local" });  // -> local copy only
```

`both` never fabricates data: a single failing side is logged and tolerated, and
only a total failure raises an error.

### Python

```python
import asyncio

from ariacompute_agent import Agent, Runner, Session, function_tool


@function_tool
def history_fun_fact() -> str:
    """Return a short history fact."""
    return "Sharks are older than trees."


agent = Agent(
    name="History tutor",
    instructions="Answer history questions clearly and concisely.",
    model="gpt-4o-mini",
    tools=[history_fun_fact],
    # memory backend: cloud | local (aria memo) | both
    memory_backend="both",
    memo_db="~/.ariacompute/memo.db",
)


async def main() -> None:
    session = Session.create(agent)

    # 1) One-shot run.
    first = await Runner.run(agent, "When did the Roman Empire fall?", session=session)
    print(first.final_output)

    # Long-term memory lives in the session's context store (Postgres +
    # pgvector): `memorize` persists a fact that every later turn can recall.
    session.memorize("user_name", "Ada")

    # 2) Continue the same conversation by reusing the session — the reply can
    #    draw on both the earlier turns and the remembered fact.
    second = await Runner.run(agent, "What is my name?", session=session)
    print(second.final_output)
    print(session.recall("user_name"))                   # -> "Ada"
    print(session.recall("user_name", backend="local"))  # local (aria memo) copy only

    # 3) Streaming run.
    streamed = await Runner.run_streamed(agent, "Tell me something surprising", session=session)
    async for event in streamed.events:
        if event["type"] == "agent.turn.output_text.delta":
            print(event["delta"], end="")
        elif event["type"] == "agent.turn.approval.resolved":
            print(f"\n[tool {event['tool']}: {event['decision']} - {event.get('reason') or ''}]")
        elif event["type"] == "agent.turn.usage":
            print(f"\n[tokens: {event['usage']['total_tokens']}]")
    final = await streamed.completed
    print(final.final_output)
    if final.usage:
        print(f"tokens: {final.usage.total_tokens}")


asyncio.run(main())
```

### Error handling & limits

Transports normalize failures into a typed `AriaError` (`kind ∈ {auth, network,
api, config, unknown}`): `401/403` → `auth`, other non-2xx → `api` (with the HTTP
`status`), a connection failure → `network`, and local misuse (e.g. a non-positive
`max_turns` or an unknown memory backend) → `config`. A stream that ends without a
single `agent.*` frame is `unknown` — never an empty answer. Branch on `err.kind`
instead of matching message text.

```python
from ariacompute_agent import AriaError, AriaErrorKind

try:
    final = await Runner.run(agent, "hi", session=session, max_turns=1)
except AriaError as err:
    if err.kind == AriaErrorKind.AUTH:
        print("check your API key")
    elif err.kind == AriaErrorKind.CONFIG:
        print("bad option:", err)
```

Read-only session endpoints (no context mutation) are also available:
`session.get_items()`, `session.get_turns()` and `session.get_title()` (GET
`…/items`, `…/turns`, `…/title`). The title is the `title_set` projection; a
delegated child session (`<parent>::sub::<id>`) is readable through its parent's
ownership, while an untitled session returns 404.
The JS SDK mirrors all of the above (`AriaError`, `run`/`runStreamed` `maxTurns`,
`Session.getItems()`/`getTurns()`/`getTitle()`).

### TypeScript

```typescript
import { Agent, run, runStreamed, tool, Session } from "@ariacompute/agent";

const historyFunFact = tool({
  name: "history_fun_fact",
  description: "Return a short history fact.",
  parameters: { type: "object", properties: {} },
});

const agent = new Agent({
  name: "History tutor",
  instructions: "Answer history questions clearly and concisely.",
  model: "gpt-4o-mini",
  tools: [historyFunFact],
  // memory backend: cloud | local (aria memo) | both
  memory: { backend: "both", memoDb: "~/.ariacompute/memo.db" },
});

const session = await Session.create(agent);

// 1) One-shot run.
const first = await run(agent, "When did the Roman Empire fall?", { session });
console.log(first.finalOutput);

// Long-term memory lives in the session's context store (Postgres + pgvector):
// `memorize` persists a fact that every later turn can recall.
await session.memorize("user_name", "Ada");

// 2) Continue the same conversation by reusing the session — the reply can draw
//    on both the earlier turns and the remembered fact.
const second = await run(agent, "What is my name?", { session });
console.log(second.finalOutput);
console.log(await session.recall("user_name"));                        // -> "Ada"
console.log(await session.recall("user_name", { backend: "local" }));  // aria memo only

// 3) Streaming run.
const streamed = await runStreamed(agent, "Tell me something surprising", { session });
for await (const event of streamed.events) {
  if (event.type === "agent.turn.output_text.delta") {
    process.stdout.write(event.delta);
  } else if (event.type === "agent.turn.approval.resolved") {
    console.log(`\n[tool ${event.tool}: ${event.decision}]`);
  } else if (event.type === "agent.turn.usage") {
    console.log(`\n[tokens: ${event.usage.total_tokens}]`);
  }
}
const result = await streamed.completed;
console.log(result.finalOutput);
if (result.usage) console.log(`tokens: ${result.usage.totalTokens}`);
```

### Rust (cloud client)

The Rust surface is the same HTTP client as JS/Python: no runtime, no sandbox,
no model key — just the cloud API. `run` creates a session when none is passed,
`memorize`/`recall` pick `cloud` | `local` (aria memo CLI) | `both`, and
`run_streamed` delivers every `agent.*` frame to a listener.

```rust
use aria_agent_ffi::{
    AriaAgent, AriaAgentConfig, ClientOptions, MemoryConfig, StreamEvent, StreamListener,
};

struct Printer;

impl StreamListener for Printer {
    fn on_event(&self, event: StreamEvent) {
        match event.kind.as_str() {
            "agent.turn.output_text.delta" => print!("{}", event.delta.unwrap_or_default()),
            "agent.turn.usage" => eprintln!("\n[tokens]"),
            _ => {}
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = AriaAgent::new(AriaAgentConfig {
        name: "History tutor".into(),
        instructions: Some("Be concise.".into()),
        model: None,
        id: None,
        client: Some(ClientOptions {
            base_url: Some("http://localhost:3000".into()),
            api_key: Some(std::env::var("ARIA_AGENT_API_KEY")?),
            beta_header: None,
        }),
        // cloud | local (aria-memo CLI) | both; per-call `backend` overrides it.
        memory: Some(MemoryConfig {
            backend: Some("both".into()),
            memo_bin: None,
            memo_db: Some("~/.ariacompute/memo.db".into()),
        }),
    })?;

    // 1) One-shot run — a session is created server-side and returned.
    let first = agent.run("When did the Roman Empire fall?".into(), None)?;
    println!("{}", first.final_output);

    // 2) Reuse the session so later turns see the earlier ones.
    let session = agent.create_session()?;
    session.memorize("user_name".into(), "Ada".into(), None)?;
    println!("{}", agent.run("What is my name?".into(), Some(session.clone()))?.final_output);
    println!("{:?}", session.recall("user_name".into(), None)?);  // -> Some("Ada")
    println!("{:?}", session.recall("user_name".into(), Some("local".into()))?);

    // 3) Streaming run — every frame reaches the listener in wire order.
    let result = agent.run_streamed(
        "Tell me something surprising".into(),
        Some(session),
        Box::new(Printer),
        None,
    )?;
    println!("\n{}", result.final_output);
    Ok(())
}
```

### Swift (cloud client)

Embed `libaria-agent_ffi` and the generated `AriaComputeAgent` module (see
`bindings/swift/README.md`). Only HTTP is used, so the same code runs on iOS.

```swift
import AriaComputeAgent

final class Printer: StreamListener {
    func onEvent(event: StreamEvent) {
        switch event.kind {
        case "agent.turn.output_text.delta": print(event.delta ?? "", terminator: "")
        case "agent.turn.usage": print("\n[tokens]")
        default: break
        }
    }
}

do {
    let agent = try AriaAgent(config: AriaAgentConfig(
        name: "History tutor",
        instructions: "Be concise.",
        model: nil,
        id: nil,
        client: ClientOptions(
            baseUrl: "http://localhost:3000",
            apiKey: ProcessInfo.processInfo.environment["ARIA_AGENT_API_KEY"],
            betaHeader: nil
        ),
        // cloud | local (aria-memo CLI) | both.
        memory: MemoryConfig(
            backend: "both",
            memoBin: nil,
            memoDb: "~/.ariacompute/memo.db"
        )
    ))

    // 1) One-shot run — a session is created server-side and returned.
    print(try agent.run(input: "When did the Roman Empire fall?", session: nil).finalOutput)

    // 2) Reuse the session so later turns see the earlier ones.
    let session = try agent.createSession()
    try session.memorize(key: "user_name", value: "Ada", backend: nil)
    print(try agent.run(input: "What is my name?", session: session).finalOutput)
    print(try session.recall(key: "user_name", backend: nil) ?? "")  // -> "Ada"
    print(try session.recall(key: "user_name", backend: "local") ?? "")

    // 3) Streaming run — every frame reaches the listener in wire order.
    let result = try agent.runStreamed(
        input: "Tell me something surprising",
        session: session,
        listener: Printer(),
        maxTurns: nil
    )
    print(result.finalOutput)
} catch {
    print(error)
}
```

### Kotlin (cloud client)

Add the `com.ariacompute:agent` artifact and the `libaria-agent_ffi` native
library (see `bindings/kotlin/agent-sdk/README.md`). Only HTTP is used, so the
same code runs on Android.

```kotlin
import com.ariacompute.agent.uniffi.aria_agent_ffi.*

class Printer : StreamListener {
    override fun onEvent(event: StreamEvent) {
        when (event.kind) {
            "agent.turn.output_text.delta" -> print(event.delta ?: "")
            "agent.turn.usage" -> println("\n[tokens]")
        }
    }
}

fun main() {
    val agent = AriaAgent(
        AriaAgentConfig(
            name = "History tutor",
            instructions = "Be concise.",
            model = null,
            id = null,
            client = ClientOptions(
                baseUrl = "http://localhost:3000",
                apiKey = System.getenv("ARIA_AGENT_API_KEY"),
                betaHeader = null
            ),
            // cloud | local (aria-memo CLI) | both.
            memory = MemoryConfig(
                backend = "both",
                memoBin = null,
                memoDb = "~/.ariacompute/memo.db"
            )
        )
    )

    // 1) One-shot run — a session is created server-side and returned.
    println(agent.run("When did the Roman Empire fall?", null).finalOutput)

    // 2) Reuse the session so later turns see the earlier ones.
    val session = agent.createSession()
    session.memorize("user_name", "Ada", null)
    println(agent.run("What is my name?", session).finalOutput)
    println(session.recall("user_name", null))  // -> "Ada"
    println(session.recall("user_name", "local"))

    // 3) Streaming run — every frame reaches the listener in wire order.
    val result = agent.runStreamed("Tell me something surprising", session, Printer(), null)
    println(result.finalOutput)
}
```

#### Building the Android AAR

The Kotlin binding in `bindings/kotlin/agent-sdk` is published as an Android
library AAR that bundles the native `libaria_agent_ffi.so` for `arm64-v8a` and
`x86_64`. To build it from source:

1. Prerequisites: the Android SDK + NDK (set `ANDROID_NDK`), plus the Rust
   Android targets:
   ```bash
   rustup target add aarch64-linux-android x86_64-linux-android
   ```
   Add an `agent/.cargo/config.toml` that points those two targets at the NDK
   clang linker (API 24), e.g.
   ```toml
   [target.aarch64-linux-android]
   linker = "aarch64-linux-android24-clang"
   [target.x86_64-linux-android]
   linker = "x86_64-linux-android24-clang"
   ```
   (the NDK `toolchains/llvm/prebuilt/linux-x86_64/bin` dir must be on `PATH`
   when building).
2. Compile and stage the native cdylib for each ABI:
   ```bash
   cargo build -p ariacompute-agent --target aarch64-linux-android --release
   cargo build -p ariacompute-agent --target x86_64-linux-android --release
   cp target/aarch64-linux-android/release/libaria_agent_ffi.so \
      bindings/kotlin/agent-sdk/src/main/jniLibs/arm64-v8a/
   cp target/x86_64-linux-android/release/libaria_agent_ffi.so \
      bindings/kotlin/agent-sdk/src/main/jniLibs/x86_64/
   ```
   (Regenerate the Kotlin bindings with `just ffi` only if the FFI surface
   changed.)
3. Install a JDK 17 and Gradle 8.9+ (this project ships no Gradle wrapper, so
   both `java` and `gradle` must be on `PATH`). If you see
   `JAVA_HOME is not set and no 'java' command could be found`, install a JDK
   and export `JAVA_HOME`:
   ```bash
   # Ubuntu/Debian: OpenJDK 17 (AGP 8.5 requires JDK 17)
   sudo apt install openjdk-17-jdk
   export JAVA_HOME=$(dirname $(dirname $(readlink -f $(which java))))
   # persist it, e.g. add the export above to ~/.bashrc
   ```
   Then install Gradle 8.9+ — note the apt package is an old 4.x and **will not
   work**:
   ```bash
   # Ubuntu/Debian: the snap channel ships a current 8.x (e.g. 8.14.4)
   sudo snap install gradle  # add --classic if confinement blocks it
   # Alternative — official tarball (pin any 8.9+):
   #   curl -L https://services.gradle.org/distributions/gradle-8.9-bin.zip -o /tmp/gradle.zip
   #   unzip /tmp/gradle.zip -d /opt && export PATH="/opt/gradle-8.9/bin:$PATH"
   # Alternative — SDKMAN:
   #   sdk install gradle 8.9
   ```
   (Do **not** run `sudo apt install gradle` — it provides Gradle 4.4.1, too old
   for this project's AGP/Kotlin plugin versions.)
4. Assemble the AAR:
   ```bash
   cd bindings/kotlin/agent-sdk
   gradle assembleRelease
   ```
   The output is `bindings/kotlin/agent-sdk/build/outputs/aar/agent-sdk-release.aar`.
   The AAR version follows the agent git tag (e.g. `v1.2.0` → `1.2.0`); override
   it with the `ARIA_VERSION` environment variable.

Consumers resolve the AAR from a `flatDir` repo or a Maven repository (see the
`cockpit/app` README for an example).

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🔏 Trust store security

The 2026-10 eMudhra (emSign) intermediate CA key compromise is handled in two
layers for the agent:

- **Rust (cloud + CLI binaries).** Outbound TLS (provider API calls, `upgrade`
  downloads from GitHub/Gitee Releases) goes through `reqwest` + `rustls-tls`,
  whose trust store is the compiled-in **`webpki-roots`**, pinned `>=1.0.10` in
  `aria-agent-cloud` so the compromised emSign roots are removed. The binaries do
  not use the system bundle, so the lock bump is the decisive fix.
- **JS SDK.** Built and tested with **bun >= 1.4.4**; it performs no outbound TLS
  itself beyond the fetch it makes to the cloud service.
- **Python SDK.** Bundles no trust store and relies on the host / container
  `ca-certificates`.

`scripts/check-emudhra.sh` scans a system or image CA bundle offline for the known
root / intermediate SHA-256 fingerprints (exit `0` clean, `1` hit, `2` unreadable):

```bash
scripts/check-emudhra.sh /etc/ssl/certs/ca-certificates.crt
```

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🛡️ Governed execution

Every outbound request crosses one boundary that decides, substitutes and records.
Defaults are closed and come from `AgentConfig` only, so a CLI and a
cloud deployment cannot disagree about what the default is.

| Variable | Default | Meaning |
|---|---|---|
| `AGENT_EGRESS_MODE` | `deny_all` | `deny_all` \| `allowlist` \| `denylist` |
| `AGENT_EGRESS_ALLOW` | *(empty)* | hosts reachable when mode is `allowlist` (`*.` = sub-domains) |
| `AGENT_EGRESS_DENY` | *(empty)* | hosts always refused |
| `AGENT_EGRESS_ENFORCEMENT` | `proxy` | `proxy` \| `network` \| `off` (needs `AGENT_ALLOW_UNENFORCED_EGRESS=1`) |
| `AGENT_AUDIT_DIR` | _(empty)_ → `<workspace>/.aria/audit` | JSONL sink; `/v1/governance/audit/export` returns NDJSON |
| `AGENT_AUDIT_RETENTION_DAYS` | `30` | `0` keeps forever |
| `AGENT_METERING` | `on` | state-based usage ledger |
| `AGENT_COMPRESSION` | `on` | rule-based projection of tool output for the model |
| `AGENT_CREDENTIALS_DIR` | *(unset → disabled)* | encrypted-at-rest credential store |
| `AGENT_CREDENTIAL_MASTER_KEY` | *(required if the store is on)* | passphrase for CHACHA20-POLY1305 |
| `AGENT_CREDENTIAL_MASTER_KEY_ENV` | `AGENT_CREDENTIAL_MASTER_KEY` | name of the environment variable that holds the passphrase, for deployments that inject it under a different name |
| `AGENT_CREDENTIAL_ROTATE_SECS` | `0` | credential rotation period in seconds; `0` disables rotation |
| `AGENT_IDLE_TIMEOUT_SECS` | `900` | `0` disables suspension; five signals must all be quiet |

Credentials are written as the literal `${credential}`; the plaintext is only
substituted at the boundary and never enters logs, events or audit records.

### `AGENT_EGRESS_ENFORCEMENT=network`

`proxy` (default) only sets `HTTP_PROXY`; a binary that ignores it still has a
default route. `network` removes the route instead:

```bash
just egress-proxy-image                     # build the sidecar image
export AGENT_EGRESS_ENFORCEMENT=network
export EGRESS_PROXY_IMAGE=aria-egress-proxy:local
export CONTROL_PLANE_URL=http://cloud:3000  # how the proxy reaches this service
docker compose up
```

On boot the service creates the `aria-egress` network (`internal: true`), starts
the proxy container on it **and** on the compose network, then runs every sandbox
with `network_mode=aria-egress`. The proxy is not a compose service.

| Variable | Meaning |
|---|---|
| `EGRESS_PROXY_IMAGE` | sidecar image; required for `network` |
| `AGENT_EGRESS_NETWORK` | internal network name (default `aria-egress`) |
| `AGENT_EGRESS_PROXY_URL` | address reachable from the sandbox (default `http://aria-egress-proxy:8080`) |
| `CONTROL_PLANE_URL` | how the proxy reaches this service |
| `EGRESS_PROXY_TOKEN` | shared secret; generated per boot when unset |
| `EGRESS_PROXY_PEER_TTL_SECS` | peer identity cache TTL for the sidecar (default `10`) |

If any step fails (no network, no image, proxy not running, not inside a
container) the service **refuses to start** with the next action in the message.
There is no fallback to `proxy` mode.

Identity is the sandbox's source IP: containers are labelled `aria.tenant_id` /
`aria.session_id` at creation, and the proxy asks `/internal/egress/whoami`. An
unlabelled peer is refused, and a request naming a principal that does not own
that IP is rejected — a compromised proxy cannot read another tenant's policy.
The answer is cached for 10s (max 4096 peers): Docker recycles IPs, so a longer
TTL would eventually attribute traffic to the wrong tenant.

Coverage and limits:

* Every sandbox backend is wrapped, including the browser backend's inner Docker
  sandbox, so `shell`, `job_*`, the agent loop and `browser_*` all land on the
  internal network. The wrapper is applied at the runtime's construction point
  too, so an embedded `Agent` with `egress_network` set cannot skip it. `cube`
  refuses a custom network and identity labels instead of silently keeping a
  default route; it does forward the proxy environment (`-e`).
* `term_*` is **not registered** under `network` enforcement: it drives a PTY in
  the control-plane process, which no container network can cover. Use `proxy`
  mode if an interactive host terminal is required.
* A restored checkpoint joins the internal network with the authenticated
  principal's tenant label; without `AGENT_EGRESS_NETWORK` the restore fails
  rather than deriving a container on the default bridge. The restored sandbox
  also gets the proxy environment, in `proxy` mode too, so it never goes direct.

`GET /v1/governance/status` reports `enforcement: "network"` plus the proxy's
container, network, image and boot-time health.

Cloud endpoints (all `principal_id`-scoped): `GET|PUT|PATCH
/v1/governance/egress-policy` (revision-guarded; stale writes get `409`), `GET
/v1/governance/audit`, `GET /v1/governance/audit/export`, `GET
/v1/governance/usage`, `GET /v1/governance/status`, credential metadata under
`/v1/governance/credentials`, and checkpoints under `/v1/governance/checkpoints`
(restore derives a new sandbox and passes the same quota gate as creation).

Migration note: the four governance tables (`egress_policies`, `audit_events`,
`usage_events`, `sandbox_checkpoints`) are created with `CREATE TABLE IF NOT
EXISTS` in `ensure_schema`, so an existing database needs no manual step. An
`allowlist` policy with no hosts boots fine but denies every request — use
`deny_all` to say that on purpose.

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 📚 Documentation

Detailed design records live under [`docs/adr/`](./docs/adr), written in English:

| Document | What it covers |
|---|---|
| [FFI boundary](./docs/adr/0002-ffi-boundary.md) | Stable `ariacompute-agent` exported surface and regeneration rule |
| [Sandbox providers](./docs/adr/0003-sandbox-providers.md) | Docker / Kata / Cube / Browser provider traits |
| [Memo storage](./docs/adr/0004-memo-storage.md) | `aria-agent-memo` on-device store schema |
| [Multi-tenant API keys](./docs/adr/0007-multi-tenant-api-keys.md) | Admin / tenant key model and `POST /v1/api-keys` |
| [SDK interface](./docs/adr/0008-sdk-interface.md) | Session-less happy-path SDK ergonomics |
| [OpenAI beta Agents API](./docs/adr/0009-openai-beta-agents-api.md) | Streaming event envelope contract |
| [Context storage pgvector](./docs/adr/0010-context-storage-pgvector.md) | Why pgvector is mandatory for cloud context |
| [Memory backend switch](./docs/adr/0011-memory-backend-switch.md) | `cloud` / `local` / `both` selection and merge |
| [Browser sandbox](./docs/adr/0012-browser-sandbox.md) | Six-image `BrowserCatalog` and `BrowserSandbox` |
| [Agent–memo integration](./docs/adr/0013-agent-memo-integration.md) | Memory graph, auto-compact, memo tool family |
| [DeepSeek Harness runtime](./docs/adr/0019-dsh-runtime.md) | `dsh` as the default `agent_loop` provider |
| [Governed egress](./docs/adr/0025-governed-egress-single-boundary.md) | One decide/substitute/audit boundary, deny-by-default |
| [SDK cloud client](./docs/adr/0020-sdk-cloud-client.md) | Why the native SDK embeds no runtime |
| [Audit schema & retention](./docs/adr/0021-audit-event-schema-and-retention.md) | Audit event fields, JSONL sink, retention |
| [Checkpoints & metering](./docs/adr/0022-sandbox-checkpoint-and-metering.md) | Sandbox snapshots and state-based usage |
| [Idle & rotation](./docs/adr/0023-idle-detection-and-credential-rotation.md) | Five-signal idle detection, credential rotation |
| [Network enforcement](./docs/adr/0024-network-enforcement-sidecar.md) | Internal network + secretless egress proxy |
| [Recursive self-improvement](./docs/adr/0026-recursive-self-improvement.md) | Ledger diagnosis, deterministic proposals, replay validation, revision-guarded playbooks, receipted real re-run |
| [dsh sandbox runtime](./docs/adr/0027-dsh-sandbox-runtime.md) | Containerised harness: keep-alive + attached exec, named-volume session workspace, fail-closed preflight |
| [A2A server](./docs/adr/0028-a2a-server.md) | The cloud as an A2A remote agent: public cards, JSON-RPC, SSE replay, cancel evicting the runtime |

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 📝 License

MIT. See [LICENSE](./LICENSE).

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🤝 Engineering conventions

This repository follows the Harness Engineering philosophy: the repository is the agent's context, and the documents that govern it are versioned alongside the code.

- [`AGENTS.md`](./AGENTS.md) — agent engineering context entry and directory index
- [`requirements.md`](./requirements.md) — requirements spec (feature boundaries / exceptions / acceptance criteria), human-review-gated before implementation
- [`task.md`](./task.md) — implementation task checklist

<div align="right">

[![][back-to-top]](#readme-top)

</div>

---

<div align="center">

Copyright © 2026 [ariacompute][org]. <br/>
This project is [MIT](./LICENSE) licensed.

</div>

<!-- LINK GROUP -->

[org]: https://github.com/ariacompute
[release-link]: https://github.com/ariacompute/agent/releases
[language-link]: ./Cargo.toml
[license-link]: ./LICENSE
[lastcommit-link]: https://github.com/ariacompute/agent/commits
[sdks-link]: #-sdk-examples
[tests-link]: ./justfile
[docs-link]: ./docs/adr

[release-shield]: https://img.shields.io/github/v/release/ariacompute/agent?style=flat-square&labelColor=black&color=555
[language-shield]: https://img.shields.io/badge/language-Rust%20%2F%20Swift%20%2F%20Kotlin-555?style=flat-square&labelColor=black
[license-shield]: https://img.shields.io/github/license/ariacompute/agent?style=flat-square&labelColor=black&color=555
[lastcommit-shield]: https://img.shields.io/github/last-commit/ariacompute/agent?style=flat-square&labelColor=black&color=555
[sdks-shield]: https://img.shields.io/badge/SDKs-5_languages-555?style=flat-square&labelColor=black
[tests-shield]: https://img.shields.io/badge/tests-cargo_test--workspace-555?style=flat-square&labelColor=black
[docs-shield]: https://img.shields.io/badge/docs-22_ADRs-555?style=flat-square&labelColor=black

[back-to-top]: https://img.shields.io/badge/-BACK_TO_TOP-151515?style=flat-square
