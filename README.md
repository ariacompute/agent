# agent

A layered agent platform built on OpenAI's [`codex`](https://github.com/openai/codex)
harness, exposing agents as (a) a **Rust + Cloud API** and (b)
**native SDKs for Swift / Kotlin**.

## Modules

| Module | What it is |
|--------|------------|
| **codex submodule** | `openai/codex` at `codex/` (harness, sandboxing, memories). |
| **aria-agent-memo** | Unified **context memory** (memo). Local/embedded store, **not Postgres**. |
| **aria-agent-sandbox** | Pluggable `Sandbox`: Docker (default) / Kata / Cube. |
| **aria-agent-core** | Unified agent runtime: `recall → model → memorize`, tools in a sandbox. |
| **ariacompute-agent** | UniFFI `cdylib` (`libaria-agent_ffi`): `SdkAgent` / `SdkSession` / `create_agent`. |
| **aria-agent-cloud** | axum service, Postgres metadata only, OpenAI Agents API. `/v1/agents`, `/v1/sessions/s1/runs`, SSE `/v1/sessions/s1/runs/stream`. |
| **bindings** | `bindings/swift` (SwiftPM) and `bindings/kotlin` (Android). |

> **Storage boundary:** memo is the *only* context store and never uses
> Postgres. Postgres is used *only* by `aria-agent-cloud` for `agents`/`runs`
> metadata.

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
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `postgres` / `postgres` / `agent` | Postgres credentials + database (metadata store). |
| `DATABASE_URL` | `postgres://postgres:postgres@postgres:5432/agent` | Connection string. The host `postgres` is the compose service name. |
| `OPENAI_API_KEY` | _(empty)_ | OpenAI key for model calls. Leave empty only for stub/offline runs. |
| `AGENT_CLOUD_API_KEY` | _(empty)_ | Bootstrap **admin** key. When set, it is the Admin principal (present as `Authorization: Bearer <key>` / `ApiKey <key>`); admins mint per-tenant keys via `POST /v1/api-keys`. Requests without a valid key get `401`. Empty = open (`default` tenant). |
| `AGENT_MEMO_BACKEND` | `memory` | Memo backend: `memory` (ephemeral) or `memo` (persistent sled DB). |
| `MEMO_DIR` | `/app/.memo` | Directory for the persistent memo store (used when `AGENT_MEMO_BACKEND=memo`). |
| `REEF_DIR` | `/app/.reef` | Reef stores (records / feedback / git-versioned harness). |
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

### 3. Memo backend

The conversational context store (`agent-memo`) is selected with
`AGENT_MEMO_BACKEND`:

- `memory` (default) — in-memory sled; context is lost on restart.
- `memo` — persistent sled DB under `MEMO_DIR`. The compose `memodata` volume
  keeps it across restarts.

```bash
# Persistent conversational memory
AGENT_MEMO_BACKEND=memo docker compose up --build
```

> Reef data (records / feedback / harness) is always persisted to the `reefdata`
> volume regardless of the memo backend.

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
| `pgdata` | Postgres | `agents` / `runs` metadata. |
| `reefdata` | `/app/.reef` | Reef stores (always persisted). |
| `memodata` | `/app/.memo` | Persistent memo (only used when `AGENT_MEMO_BACKEND=memo`). |

## SDK examples

There are two ways to use an Aria Agent:

- **Cloud API (HTTP)** — a language-agnostic REST endpoint. Call it from
  Python, Rust, TypeScript, or any HTTP client. Streaming is SSE emitting
  **OpenAI Responses API events** (`response.created`,
  `response.output_text.delta`, `response.function_call_arguments.delta`, …),
  terminated by `response.completed` — see
  [OpenAI Agents API compatibility](#openai-agents-api-compatibility).
- **Native SDK (in-process)** — the UniFFI bindings embed the runtime directly
  in Swift / Kotlin apps (no server, no network).

All cloud examples assume a running service (see Quick start) and, when
`AGENT_CLOUD_API_KEY` is set, an `Authorization: Bearer <key>` (or
`ApiKey <key>`) header.

In the run request bodies you may use the agent's human-friendly name `agent`
instead of `agent_id` (the run is resolved per principal). The **session** is the
path `{id}` segment of `/v1/sessions/{id}/runs` — it scopes the conversation in
the cloud's memo store, so there is no `session` field in the body. The endpoint
always streams; call `/v1/sessions/{id}/runs` (without `/stream`) for a single
blocking reply.

### Python (cloud API)

```python
import os, json, requests

BASE = os.environ.get("ARIA_AGENT_BASE", "http://localhost:3000")
API_KEY = os.environ.get("AGENT_CLOUD_API_KEY")
headers = {"Authorization": f"Bearer {API_KEY}"} if API_KEY else {}

agent = requests.post(f"{BASE}/v1/agents", json={"name": "my-agent"}, headers=headers).json()
agent_name = agent["name"]

# one-shot run
run = requests.post(
    f"{BASE}/v1/sessions/s1/runs",
    json={"agent": agent_name, "input": "hello"},
    headers=headers,
).json()
print(run["output"])

# streaming run — OpenAI Responses API events
with requests.post(
    f"{BASE}/v1/sessions/s1/runs/stream",
    json={"agent": agent_name, "input": "tell me a joke"},
    headers=headers,
    stream=True,
) as r:
    for raw in r.iter_lines():
        if not raw:
            continue
        line = raw.decode() if isinstance(raw, bytes) else raw
        if not line.startswith("data:"):
            continue
        payload = line[len("data:"):].strip()
        event = json.loads(payload)
        if event["type"] == "response.output_text.delta":
            print(event["delta"], end="", flush=True)
        elif event["type"] == "response.completed":
            # terminal event
            print("\nreceipt:", event["response"]["metadata"]["reef_record_id"])
            break
```

### Rust (cloud API)

Add `reqwest` (with the `json` and `stream` features), `tokio`, `serde`, `serde_json`, and `anyhow`:

```rust
use reqwest::Client;
use serde::Deserialize;

#[derive(Deserialize)] struct Agent { name: String }
#[derive(Deserialize)] struct Run  { output: String }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::var("ARIA_AGENT_BASE")
        .unwrap_or_else(|_| "http://localhost:3000".into());
    let client = Client::new();
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(key) = std::env::var("AGENT_CLOUD_API_KEY") {
        headers.insert(reqwest::header::AUTHORIZATION, format!("Bearer {key}").parse()?);
    }

    let agent: Agent = client.post(format!("{base}/v1/agents"))
        .headers(headers.clone())
        .json(&serde_json::json!({"name": "my-agent"}))
        .send().await?.json().await?;

    let run: Run = client.post(format!("{base}/v1/sessions/s1/runs"))
        .headers(headers)
        .json(&serde_json::json!({"agent": agent.name, "input": "hello"}))
        .send().await?.json().await?;

    println!("{}", run.output);

    // streaming run — OpenAI Responses API events
    let mut res = client.post(format!("{base}/v1/sessions/s1/runs/stream"))
        .headers(headers)
        .json(&serde_json::json!({"agent": agent.name, "input": "tell me a joke"}))
        .send().await?;
    let mut buf = String::new();
    while let Some(chunk) = res.chunk().await? {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buf.find('\n') {
            let line = buf[..idx].trim().to_string();
            buf.drain(..=idx);
            let Some(payload) = line.strip_prefix("data:") else { continue };
            let event: serde_json::Value = serde_json::from_str(payload.trim())?;
            if event["type"] == "response.output_text.delta" {
                print!("{}", event["delta"].as_str().unwrap_or_default());
            } else if event["type"] == "response.completed" {
                return Ok(()); // terminal event
            }
        }
    }
    Ok(())
}
```

> In Rust you can also depend on `agent-core` / `ariacompute-agent` directly to
> run the runtime in-process instead of over HTTP.

### TypeScript (cloud API)

```typescript
const base = process.env.ARIA_AGENT_BASE ?? "http://localhost:3000";
const apiKey = process.env.AGENT_CLOUD_API_KEY;
const headers: Record<string, string> = {
  "Content-Type": "application/json",
  ...(apiKey ? { Authorization: `Bearer ${apiKey}` } : {}),
};

// create + one-shot run
const agent = await fetch(`${base}/v1/agents`, {
  method: "POST", headers, body: JSON.stringify({ name: "my-agent" }),
}).then((r) => r.json<{ name: string }>());

const run = await fetch(`${base}/v1/sessions/s1/runs`, {
  method: "POST", headers,
  body: JSON.stringify({ agent: agent.name, input: "hello" }),
}).then((r) => r.json<{ output: string }>());
console.log(run.output);

// streaming run — OpenAI Responses API events
const res = await fetch(`${base}/v1/sessions/s1/runs/stream`, {
  method: "POST", headers,
  body: JSON.stringify({ agent: agent.name, input: "tell me a joke" }),
});
const reader = res.body!.getReader();
const decoder = new TextDecoder();
let buf = "";
outer: for (;;) {
  const { value, done } = await reader.read();
  if (done) break;
  buf += decoder.decode(value, { stream: true });
  let idx;
  while ((idx = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, idx).trim();
    buf = buf.slice(idx + 1);
    if (!line.startsWith("data:")) continue;
    const event = JSON.parse(line.slice(5).trim());
    if (event.type === "response.output_text.delta") {
      process.stdout.write(event.delta);
    } else if (event.type === "response.completed") {
      // terminal event
      console.log("\nreceipt:", event.response.metadata.reef_record_id);
      break outer;
    }
  }
}
```

### Swift

Embed `libaria-agent_ffi` and the generated `AriaAgent` module (see
`bindings/swift/README.md`) to run the agent **in-process**, or consume the
cloud stream over HTTP:

```swift
import AriaAgent

// Native SDK — the runtime embedded in-process (no server, no network)
let agent = createAgent(AgentConfig(
    session: "default",
    agentName: "agent",
    sandboxProvider: "docker",
    model: "gpt-4o-mini"))
let reply = agent.run("hello")
let session = agent.session()
session.memorize(key: "fact1", value: "the moon is cheese")
print(session.recall(key: "fact1") ?? "")

// Cloud streaming (OpenAI-compatible): consume /v1/sessions/s1/runs/stream response.* events
let base = ProcessInfo.processInfo.environment["ARIA_AGENT_BASE"] ?? "http://localhost:3000"
var request = URLRequest(url: URL(string: "\(base)/v1/sessions/s1/runs/stream")!)
request.httpMethod = "POST"
request.setValue("application/json", forHTTPHeaderField: "Content-Type")
request.setValue("text/event-stream", forHTTPHeaderField: "Accept")
request.httpBody = try? JSONSerialization.data(withJSONObject: [
    "agent": "Agent Demo", "input": "tell me a joke"
])

let (stream, response) = try await URLSession.shared.bytes(for: request)
// The Reef receipt is also returned as the x-reef-agent-record-id header.
let receipt = (response as? HTTPURLResponse)?.value(forHTTPHeaderField: "x-reef-agent-record-id")

outer: for try await line in stream.lines {
    guard line.hasPrefix("data:") else { continue }
    let payload = line.dropFirst(5).trimmingCharacters(in: .whitespaces)
    guard let data = payload.data(using: .utf8),
          let event = try? JSONDecoder().decode(CloudEvent.self, from: data) else { continue }
    switch event.type {
    case "response.output_text.delta":
        print(event.delta ?? "", terminator: "")
    case "response.completed":
        // terminal event
        print("\nreceipt:", event.response?.metadata?.reefRecordId ?? receipt ?? "")
        break outer
    default:
        break   // response.created / response.in_progress / output_item.* …
    }
}
```

### Kotlin

Add the `com.ariacompute:agent` artifact and the `libaria-agent_ffi` native
library (see `bindings/kotlin/agent-sdk/README.md`) to run the agent
**in-process**, or consume the cloud stream over HTTP:

```kotlin
import com.ariacompute.agent.uniffi.aria_agent_ffi.*

// Native SDK — the runtime embedded in-process (no server, no network)
fun main() {
    val agent = createAgent(AgentConfig("default", "agent", "docker", "gpt-4o-mini"))
    val reply = agent.run("hello")
    val session = agent.session()
    session.memorize("fact1", "the moon is cheese")
    println(session.recall("fact1"))
}

// Cloud streaming (OpenAI-compatible): consume /v1/sessions/s1/runs/stream response.* events
val json = Json { ignoreUnknownKeys = true }
val client = OkHttpClient()
val base = System.getenv("ARIA_AGENT_BASE") ?: "http://localhost:3000"

val request = Request.Builder()
    .url("$base/v1/sessions/s1/runs/stream")
    .header("Accept", "text/event-stream")
    .post("""{"agent":"Agent Demo","input":"tell me a joke"}"""
        .toRequestBody())
    .build()

client.newCall(request).execute().use { resp ->
    // The Reef receipt is also returned as the x-reef-agent-record-id header.
    val receipt = resp.header("x-reef-agent-record-id")
    resp.body!!.byteStream().bufferedReader().useLines { lines ->
        run loop@{
            for (line in lines) {
                if (!line.startsWith("data:")) continue
                val event = json.parseToJsonElement(line.removePrefix("data:").trim()).jsonObject
                when (event["type"]?.jsonPrimitive?.content) {
                    "response.output_text.delta" ->
                        print(event["delta"]?.jsonPrimitive?.content.orEmpty())
                    "response.completed" -> {
                        // terminal event
                        println(
                            "\nreceipt: " + (event["response"]?.jsonObject
                                ?.get("metadata")?.jsonObject
                                ?.get("reef_record_id")?.jsonPrimitive?.content ?: receipt)
                        )
                        return@loop
                    }
                }
            }
        }
    }
}
```

## OpenAI Agents API compatibility

`POST /v1/sessions/s1/runs/stream` speaks the **OpenAI Responses API streaming protocol**,
so OpenAI SDKs and the OpenAI Agents SDK can consume it unchanged. Every SSE
`data:` frame is a Responses event with a `type` and a monotonic
`sequence_number`:

| Event | Payload |
|---|---|
| `response.created` | `response.id`, `response.metadata` (`agent_id`, `session`, `reef_record_id`) |
| `response.in_progress` | phase boundary; extra `phase` (`recall` / `model` / `tool_exec` / `loop_guard`) + `label` |
| `response.output_item.added` | `item` — `message` (assistant) or `function_call` |
| `response.function_call_arguments.delta` | `item_id`, `delta` (JSON argument chunk) |
| `response.output_item.done` | completed `item`; a `function_call` also carries the executed `result` |
| `response.output_text.delta` | `item_id`, `content_index`, `delta` |
| `response.output_text.done` | `item_id`, `content_index`, `text` |
| `response.completed` | `response.status = "completed"`, `response.metadata.reef_record_id` |
| `response.failed` | `response.error` |

`response.completed` is the terminal event: when a client sees it the run is
finished — there is no trailing `data: [DONE]` sentinel, so stop on
`response.completed` (or `response.failed`). Every run also returns the Reef
receipt id as the `x-reef-agent-record-id` response header — the same value
exposed as `response.metadata.reef_record_id`.

Extra fields our orchestration needs but the Responses schema does not model
(the agentic `phase`, the tool `result`, the Reef receipt id) are carried
**inside** these OpenAI-shaped frames, so strict OpenAI clients simply ignore
them. No private `aria.*` frames are emitted.

```python
# Consume it with the official OpenAI SDK (no Aria-specific parsing needed).
from openai import OpenAI
client = OpenAI(base_url=f"{BASE}/v1", api_key=API_KEY or "unused")
for event in client.responses.create(
    model="gpt-4o-mini",
    input="tell me a joke",
    extra_body={"agent": "Agent Demo"},
    stream=True,
):
    if event.type == "response.output_text.delta":
        print(event.delta, end="", flush=True)
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
