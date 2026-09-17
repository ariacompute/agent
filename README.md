# agent

A layered agent platform built on OpenAI's [`codex`](https://github.com/openai/codex)
harness, exposing agents as (a) a **Rust + Cloud API** speaking the **OpenAI beta
Agents** resource surface, (b) **JS / Python SDKs** mirroring the OpenAI Agents
SDK, and (c) **native SDKs for Swift / Kotlin**.

## Modules

| Module | What it is |
|--------|------------|
| **codex submodule** | `openai/codex` at `codex/` (harness, sandboxing, memories). |
| **aria-agent-memo** | On-device / embedded **context store** (sled) implementing `aria-agent-core::context`. |
| **aria-agent-sandbox** | Pluggable `Sandbox`: Docker (default) / Kata / Cube. |
| **aria-agent-core** | Unified agent runtime: `recall → model → memorize`, tools in a sandbox. |
| **ariacompute-agent** | UniFFI `cdylib` (`libaria-agent_ffi`): `SdkAgent` / `SdkSession` / `create_agent`. |
| **aria-agent-cloud** | axum service: Postgres + pgvector (metadata **and** context), OpenAI beta Agents API. `/v1/agents`, `/v1/agents/sessions`, SSE `/v1/agents/sessions/{id}/events/stream`. |
| **bindings** | `bindings/swift` (SwiftPM) and `bindings/kotlin` (Android). |
| **sdk/js** | npm `@ariacompute/agent` — mirrors `@openai/agents` (`Agent`, `run`, `runStreamed`, `tool`, `Session`). |
| **sdk/python** | pip `ariacompute-agent` — mirrors `openai-agents` (`Agent`, `Runner`, `function_tool`, `Session`). |

> **Storage boundary:** the **cloud** stores conversational / long-term context in
> Postgres + pgvector (`context_fragments`, per-principal) — pgvector is required
> and boot fails without it. The **on-device** SDK keeps its embedded sled store.
> See `docs/adr/0010-context-storage-pgvector.md`.

## codex submodule

This repo depends on OpenAI's [`codex`](https://github.com/openai/codex) as a git
submodule at `codex/`. The upstream `codex` workspace lives at `codex/codex-rs/`
and has **no manifest at the repo root**, so a git dependency (`package =
"codex-*"`) cannot resolve without a patch.

`patch/0001-Make-codex-a-root-cargo-workspace-edition-2024.patch` promotes the
`codex-rs` workspace definition up to the repo root (and prefixes internal
`path` dependencies with `codex-rs/`). This is exactly what the
`ariacompute/codex` fork (branch `main`) does. Apply it **inside the
`codex/` submodule** after checkout:

```bash
# 1. clone the submodule (shallow)
git submodule update --init --depth 1 codex

# 2. apply the root-workspace patch inside the submodule
git -C codex apply ../patch/0001-Make-codex-a-root-cargo-workspace-edition-2024.patch
```

After this, `codex/Cargo.toml` exists at the repo root and cargo can resolve
`codex-*` crates transitively. The patch is **not committed** in the submodule —
it lives in `patch/` at the repo root — so re-run it whenever you re-init the
submodule. To undo the patch and restore the submodule:

```bash
git -C codex checkout -- . && git -C codex clean -fd
```

> The patch is required for the deep compile-integration described in
> `docs/adr/0005-codex-integration.md`. If you only build our crates that do not
> depend on codex crates, you can skip it.

## Quick start

```bash
# 1. codex submodule + root-workspace patch (see "codex submodule" above)
git submodule update --init --depth 1 codex
git -C codex apply ../patch/0001-Make-codex-a-root-cargo-workspace-edition-2024.patch

# 2. build everything
cargo build --workspace

# 3. test
cargo test --workspace

# 4. lint (warnings as errors)
cargo clippy --workspace --all-targets -- -D warnings

# 5. generate Swift/Kotlin bindings (needs the cdylib built)
# = cargo run -p aria-agent-ffigen
just ffi

# 6. run the cloud API (needs Postgres + OpenAI key)
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/agent
export OPENAI_API_KEY=sk-...
cargo run -p aria-agent-cloud
```

## CLI

The `aria-agent` binary (crate `aria-agent-cloud`) is both the cloud server
(default subcommand `serve`) and a self-updating CLI. CLI settings live in a
small sidecar config at `~/.ariacompute/agent-cli.yml` (override the home dir
with `ARIA_COMPUTE_HOME`).

```bash
# Configure the Releases source interactively (github default, or gitee).
# The choice is written to ~/.ariacompute/agent-cli.yml as `upgrade_url`.
aria-agent setup
#   1) github  -> https://github.com/ariacompute
#   2) gitee   -> https://gitee.com/ariacompute

# Inspect the current CLI config.
aria-agent setup --status

# Remove the CLI config file.
aria-agent setup --clear

# Self-update this binary + libaria-agent_ffi from Releases.
# Reads `upgrade_url` from agent-cli.yml; pass --url to override.
aria-agent upgrade            # latest stable
aria-agent upgrade 0.7.2      # specific version
aria-agent upgrade --url https://github.com/ariacompute

# Start the cloud HTTP server (default subcommand). Reads agent-cli.yml and
# exports ARIA_AGENT_FFI_LIB to ~/.ariacompute/lib.
aria-agent serve
#   --port <n>  listen port (default: CLOUD_PORT env or 3000)
aria-agent serve --port 3000
```

> The `serve` subcommand also reads `~/.ariacompute/agent-cli.yml` and points
> `ARIA_AGENT_FFI_LIB` at `~/.ariacompute/lib` (where `upgrade` installs
> `libaria-agent_ffi`) so native SDKs can find the cdylib.

## Docker Compose deployment

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
| `OPENAI_API_KEY` | _(empty)_ | OpenAI key for model calls. Leave empty only for stub/offline runs. |
| `AGENT_CLOUD_API_KEY` | _(empty)_ | Bootstrap **admin** key. When set, it is the Admin principal (present as `Authorization: Bearer <key>` / `ApiKey <key>`); admins mint per-tenant keys via `POST /v1/api-keys`. Requests without a valid key get `401`. Empty = open (`default` tenant). |
| `API_KEY_CACHE_TTL_SEC` | `60` | How long a resolved API key stays cached before it is re-checked in Postgres. |
| `RUST_LOG` | `info` | Rust log filter: `error` \| `warn` \| `info` \| `debug` \| `trace`. |
| `CLOUD_PORT` | `3000` | Host port published for **direct** access to the cloud service (the container always listens on 3000). |
| `DOCKER_HOST` | `unix:///var/run/docker.sock` | Docker Engine API endpoint for the sandbox (DooD). Override for a remote / DinD daemon. |
| `ARIA_DOCKER_SOCKET` | _(unset)_ | Explicit sandbox socket path; takes precedence over auto-discovery (e.g. `~/.docker/run/docker.sock` on macOS). |
| `DOCKER_GID` | `998` | GID of the host `docker` group that owns `/var/run/docker.sock`. The runtime image recreates this group and adds the non-root `aria` user to it so the socket is reachable without `root`. |
| `SANDBOX_CPUS` | `1.0` | Per-tool sandbox CPU cap (e.g. `1.0`, `0.5`). Applied to Docker & Kata via the Engine API. |
| `SANDBOX_MEMORY` | `512m` | Per-tool sandbox memory cap, human-readable units (`512m`, `1g`, …). Applied to Docker & Kata; best-effort for Cube. |
| `SANDBOX_PIDS_LIMIT` | `256` | Max processes inside the sandbox (pids cgroup). Applied to Docker & Kata; best-effort for Cube. |
| `NGINX_PORT` | `80` | Host port published by the `nginx` reverse proxy (forwards to `cloud:3000`). |

> `.env` is git-ignored; `.env.example` is the committed template. Compose
> auto-loads `.env` for variable substitution and the `cloud` service also reads
> it via `env_file`, so the container process receives every variable directly.

### 2. Run

```bash
docker compose up --build
```

The cloud service waits for a healthy Postgres before starting; the schema
(`agents` / `runs`) is created automatically on boot (`ensure_schema`), so no
init scripts are needed. The API is then reachable through **either** endpoint:

- **Direct:** `http://localhost:3000` (or `http://localhost:${CLOUD_PORT}`) — the
  `cloud` service port, published for debugging / when nginx is not needed.
- **Via nginx:** `http://localhost:80` (or `http://localhost:${NGINX_PORT}`) — the
  `nginx` reverse proxy, which forwards to `cloud:3000` (recommended entrypoint;
  adds `X-Forwarded-*` headers and streams SSE without buffering).

A `GET /healthz` endpoint (served by nginx itself) can be used for liveness
checks.

### 3. Context storage (PostgreSQL + pgvector)

Conversational / long-term context lives in Postgres (`context_fragments`,
`vector(256)` with an HNSW cosine index), sharded per principal. The compose
Postgres image is `pgvector/pgvector:pg16`; the service runs
`CREATE EXTENSION IF NOT EXISTS vector` at boot and **refuses to start** if the
extension is unavailable (no silent keyword-only fallback).

The on-device SDK keeps its own embedded sled store
(`aria-agent-memo`) and is not affected by this requirement.

### 4. Sandbox via the Docker socket

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

### Volumes

| Volume | Backed by | Purpose |
|--------|-----------|---------|
| `pgdata` | Postgres (pgvector) | `agents` / `runs` metadata **and** `context_fragments` (context memory). |

## SDK examples

Every example below uses an **official SDK** — none of them hand-roll HTTP. All
SDKs mirror the OpenAI Agents SDK shape: define an `Agent`, then run it.

| Language | Install | Package / module |
|---|---|---|
| Python | `pip install ariacompute-agent` | `ariacompute_agent` (mirrors `openai-agents`) |
| TypeScript / JavaScript | `npm i @ariacompute/agent` | `@ariacompute/agent` (mirrors `@openai/agents`) |
| Rust | `cargo add ariacompute-agent` | `aria_agent_ffi` (in-process, native) |
| Swift | SwiftPM / CocoaPods `AriaAgent` | `bindings/swift` (in-process, native) |
| Kotlin | `com.ariacompute:agent` | `bindings/kotlin` (in-process, native) |

The JS / Python SDKs are thin clients over the beta Agents REST API that the
service still exposes — see
[OpenAI Agents API compatibility](#openai-agents-api-compatibility) for the wire
contract. The Rust / Swift / Kotlin SDKs embed the runtime **in-process** and keep
their context in the on-device store (no server, no network).

Cloud SDKs need a running service (see Quick start). Configure
`ARIA_AGENT_BASE_URL` (default `http://localhost:3000`) and, when
`AGENT_CLOUD_API_KEY` is set, `ARIA_AGENT_API_KEY`.

Every example follows the same three steps:

1. **one-shot run** — `run(agent, input)` / `Runner.run(agent, input)`,
2. **continue the same conversation** by reusing the session (after writing a
   long-term memory with `memorize`),
3. **streaming run** — `runStreamed` / `Runner.run_streamed`.

Long-term memory lives in the agent's own context store: `memorize(key, value)`
persists a fact (as a `long_term` context fragment) that later turns of the same
session recall semantically — even after a restart. For the cloud SDKs that store
is Postgres + pgvector; for the native SDKs it is the on-device store.

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
session.memorize("user_name", "Ada")                    # -> both backends
session.recall("user_name", backend="local")            # -> local copy only
```

```typescript
const agent = new Agent({ name: "History tutor", memory: "both", memoDb: "~/.ariacompute/memo.db" });
await session.memorize("user_name", "Ada");                       // -> both backends
await session.recall("user_name", { backend: "local" });          // -> local copy only
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
    print(session.recall("user_name"))  # -> "Ada"
    print(session.recall("user_name", backend="local"))  # local (aria memo) copy only

    # 3) Streaming run.
    streamed = await Runner.run_streamed(agent, "Tell me something surprising", session=session)
    async for event in streamed.events:
        if event["type"] == "agent.turn.output_text.delta":
            print(event["delta"], end="")
    final = await streamed.completed
    print(final.final_output)


asyncio.run(main())
```

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
  memory: "both",
  memoDb: "~/.ariacompute/memo.db",
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
console.log(await session.recall("user_name")); // -> "Ada"
console.log(await session.recall("user_name", { backend: "local" })); // aria memo only

// 3) Streaming run.
const streamed = await runStreamed(agent, "Tell me something surprising", { session });
for await (const event of streamed.events) {
  if (event.type === "agent.turn.output_text.delta") {
    process.stdout.write(event.delta);
  }
}
console.log((await streamed.completed).finalOutput);
```

### Rust (in-process)

```rust
use aria_agent_ffi::{create_agent, SdkAgentListener, SdkStreamEvent};

struct Printer;

impl SdkAgentListener for Printer {
    fn on_event(&self, event: SdkStreamEvent) {
        if let SdkStreamEvent::Token { text } = event {
            print!("{text}");
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // No session argument: the runtime assigns one isolated context scope per agent.
    // `memory` selects cloud | local (aria memo) | both; default is `local`.
    let agent = create_agent_with(SdkAgentConfig {
        agent_name: "History tutor".into(),
        model: "gpt-4o-mini".into(),
        memory: SdkMemoryConfig {
            backend: "both".into(),
            local_db_path: "~/.ariacompute/memo.db".into(),
            cloud_base_url: "http://localhost:3000".into(),
            cloud_api_key: String::new(),
        },
        ..Default::default()
    })?;

    // 1) One-shot run.
    println!("{}", agent.run("When did the Roman Empire fall?".into())?);

    // Long-term memory lives in the on-device store: `memorize` persists a fact
    // as a `long_term` fragment that later turns of this agent recall.
    let session = agent.session();
    session.memorize("user_name".into(), "Ada".into())?;

    // 2) Continue the same conversation (same agent / session scope) — the reply
    //    can draw on the earlier turn and the remembered fact.
    println!("{}", agent.run("What is my name?".into())?);
    println!("{:?}", session.recall("user_name".into(), None)?); // -> Some("Ada")
    // Per-call override: read the aria memo copy only.
    println!("{:?}", session.recall("user_name".into(), Some("local".into()))?);

    // 3) Streaming run — events are pushed to the listener.
    agent.run_stream("Tell me something surprising".into(), Box::new(Printer))?;
    Ok(())
}
```

### Swift (in-process)

Embed `libaria-agent_ffi` and the generated `AriaAgent` module (see
`bindings/swift/README.md`).

```swift
import AriaAgent

do {
    // No session argument: the runtime assigns one isolated context scope per agent.
    // `memory` selects cloud | local (aria memo) | both; default is `local`.
    let agent = try createAgentWith(config: SdkAgentConfig(
        agentName: "History tutor",
        instructions: "",
        model: "gpt-4o-mini",
        sandboxProvider: "docker",
        memory: SdkMemoryConfig(
            backend: "both",
            localDbPath: "~/.ariacompute/memo.db",
            cloudBaseUrl: "http://localhost:3000",
            cloudApiKey: ""
        )
    ))

    // 1) One-shot run.
    print(try agent.run("When did the Roman Empire fall?"))

    // Long-term memory lives in the on-device store: `memorize` persists a fact
    // as a `long_term` fragment that later turns of this agent recall.
    let session = agent.session()
    try session.memorize(key: "user_name", value: "Ada")

    // 2) Continue the same conversation (same agent / session scope) — the reply
    //    can draw on the earlier turn and the remembered fact.
    print(try agent.run("What is my name?"))
    print(try session.recall(key: "user_name", backend: nil) ?? "")   // -> "Ada"
    // Per-call override: read the aria memo copy only.
    print(try session.recall(key: "user_name", backend: "local") ?? "")

    // 3) Streaming run — events are delivered to the listener.
    try agent.runStream("Tell me something surprising", listener: Printer())
} catch {
    print(error)
}
```

### Kotlin (in-process)

Add the `com.ariacompute:agent` artifact and the `libaria-agent_ffi` native
library (see `bindings/kotlin/agent-sdk/README.md`).

```kotlin
import com.ariacompute.agent.uniffi.aria_agent_ffi.*

fun main() {
    // No session argument: the runtime assigns one isolated context scope per agent.
    // `memory` selects cloud | local (aria memo) | both; default is `local`.
    val agent = createAgentWith(
        SdkAgentConfig(
            agentName = "History tutor",
            instructions = "",
            model = "gpt-4o-mini",
            sandboxProvider = "docker",
            memory = SdkMemoryConfig(
                backend = "both",
                localDbPath = "~/.ariacompute/memo.db",
                cloudBaseUrl = "http://localhost:3000",
                cloudApiKey = ""
            )
        )
    )

    // 1) One-shot run.
    println(agent.run("When did the Roman Empire fall?"))

    // Long-term memory lives in the on-device store: `memorize` persists a fact
    // as a `long_term` fragment that later turns of this agent recall.
    val session = agent.session()
    session.memorize("user_name", "Ada")

    // 2) Continue the same conversation (same agent / session scope) — the reply
    //    can draw on the earlier turn and the remembered fact.
    println(agent.run("What is my name?"))
    println(session.recall("user_name", null))   // -> "Ada"
    // Per-call override: read the aria memo copy only.
    println(session.recall("user_name", "local"))

    // 3) Streaming run — events are delivered to the listener.
    agent.runStream("Tell me something surprising", Printer())
}
```

## OpenAI Agents API compatibility

`POST /v1/agents/sessions/{id}/events/stream` speaks the **OpenAI beta Agents
streaming protocol**, so an OpenAI client can consume it unchanged. Every SSE
`data:` frame is an `agent.*` event with `event_id`, `session_id`, `turn_id` and a
monotonic `sequence_number`:

| Event | Payload |
|---|---|
| `agent.turn.created` | `turn{id,status,session_id,agent_id}` |
| `agent.turn.in_progress` | phase boundary; extra `phase` (`recall` / `model` / `tool_exec` / `loop_guard`) + `label` |
| `agent.turn.item.added` | `item` — `message` (assistant, `role`) or `function_call` (`name`, `arguments`) |
| `agent.turn.item.done` | completed `item`; a `function_call` also carries the executed `result` |
| `agent.turn.output_text.delta` | `item_id`, `delta` |
| `agent.turn.output_text.done` | `item_id`, `text` |
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
```

### Default agent

`ensure_schema` seeds a default agent — `id: agent-demo`,
`name: Agent Demo` — so a fresh deployment (and the Aria Playground) has
something to run out of the box. Deployments seeded with the previous default
(`playground-demo`) are renamed in place on the next boot. List the agents
visible to your key with `GET /v1/agents` (admins see every agent, tenants
only their own).

## License

MIT.

## Engineering Conventions

This repository follows the Harness Engineering philosophy:

- [`AGENTS.md`](AGENTS.md): Agent engineering context entry and directory index
- [`requirements.md`](requirements.md): Requirements spec (feature boundaries/exceptions/acceptance criteria, human-review-gated)
- [`task.md`](task.md): Implementation task checklist
