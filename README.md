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
| **aria-agent-cloud** | axum service, Postgres metadata only, OpenAI Agents API. `/v1/agents`, `/v1/runs`, SSE `/v1/runs/stream`. |
| **bindings** | `bindings/swift` (SwiftPM) and `bindings/kotlin` (Android). |

> **Storage boundary:** memo is the *only* context store and never uses
> Postgres. Postgres is used *only* by `aria-agent-cloud` for `agents`/`runs`
> metadata.

## Quick start

```bash
# 1. codex submodule
git submodule update --init --depth 1 codex

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
| `AGENT_CLOUD_API_KEY` | _(empty)_ | API auth gate. When set, every request must carry `Authorization: Bearer <key>` (or `ApiKey <key>`). Empty = open. |
| `AGENT_MEMO_BACKEND` | `memory` | Memo backend: `memory` (ephemeral) or `memo` (persistent sled DB). |
| `MEMO_DIR` | `/app/.memo` | Directory for the persistent memo store (used when `AGENT_MEMO_BACKEND=memo`). |
| `REEF_DIR` | `/app/.reef` | Reef stores (records / feedback / git-versioned harness). |
| `RUST_LOG` | `info` | Rust log filter: `error` \| `warn` \| `info` \| `debug` \| `trace`. |
| `CLOUD_PORT` | `3000` | Host port published for the cloud service (container always listens on 3000). |

> `.env` is git-ignored; `.env.example` is the committed template. Compose
> auto-loads `.env` for variable substitution and the `cloud` service also reads
> it via `env_file`, so the container process receives every variable directly.

### 2. Run

```bash
docker compose up --build
```

The cloud service waits for a healthy Postgres before starting; the schema
(`agents` / `runs`) is created automatically on boot (`ensure_schema`), so no
init scripts are needed. The API is then available at `http://localhost:3000`
(or `http://localhost:${CLOUD_PORT}`).

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

### Volumes

| Volume | Backed by | Purpose |
|--------|-----------|---------|
| `pgdata` | Postgres | `agents` / `runs` metadata. |
| `reefdata` | `/app/.reef` | Reef stores (always persisted). |
| `memodata` | `/app/.memo` | Persistent memo (only used when `AGENT_MEMO_BACKEND=memo`). |

## SDK examples

There are two ways to use an Aria Agent:

- **Cloud API (HTTP)** — a language-agnostic REST endpoint. Call it from
  Python, Rust, TypeScript, or any HTTP client. Streaming is SSE
  (`data: {"token":"..."}`, terminated by `data: [DONE]`).
- **Native SDK (in-process)** — the UniFFI bindings embed the runtime directly
  in Swift / Kotlin apps (no server, no network).

All cloud examples assume a running service (see Quick start) and, when
`AGENT_CLOUD_API_KEY` is set, an `Authorization: Bearer <key>` (or
`ApiKey <key>`) header.

### Python (cloud API)

```python
import os, json, requests

BASE = os.environ.get("ARIA_AGENT_BASE", "http://localhost:3000")
API_KEY = os.environ.get("AGENT_CLOUD_API_KEY")
headers = {"Authorization": f"Bearer {API_KEY}"} if API_KEY else {}

agent = requests.post(f"{BASE}/v1/agents", json={"name": "my-agent"}, headers=headers).json()
agent_id = agent["id"]

# one-shot run
run = requests.post(
    f"{BASE}/v1/runs",
    json={"agent_id": agent_id, "session": "s1", "input": "hello"},
    headers=headers,
).json()
print(run["output"])

# streaming run
with requests.post(
    f"{BASE}/v1/runs/stream",
    json={"agent_id": agent_id, "session": "s1", "input": "tell me a joke"},
    headers=headers,
    stream=True,
) as r:
    for line in r.iter_lines():
        if not line:
            continue
        if line == "data: [DONE]":
            break
        token = json.loads(line[len("data: "):])["token"]
        print(token, end="", flush=True)
```

### Rust (cloud API)

Add `reqwest` (with `json` feature), `tokio`, `serde`, `serde_json`, and `anyhow`:

```rust
use reqwest::Client;
use serde::Deserialize;

#[derive(Deserialize)] struct Agent { id: String }
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

    let run: Run = client.post(format!("{base}/v1/runs"))
        .headers(headers)
        .json(&serde_json::json!({"agent_id": agent.id, "session": "s1", "input": "hello"}))
        .send().await?.json().await?;

    println!("{}", run.output);
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
}).then((r) => r.json<{ id: string }>());

const run = await fetch(`${base}/v1/runs`, {
  method: "POST", headers,
  body: JSON.stringify({ agent_id: agent.id, session: "s1", input: "hello" }),
}).then((r) => r.json<{ output: string }>());
console.log(run.output);

// streaming run
const res = await fetch(`${base}/v1/runs/stream`, {
  method: "POST", headers,
  body: JSON.stringify({ agent_id: agent.id, session: "s1", input: "tell me a joke" }),
});
const reader = res.body!.getReader();
const decoder = new TextDecoder();
let buf = "";
for (;;) {
  const { value, done } = await reader.read();
  if (done) break;
  buf += decoder.decode(value, { stream: true });
  let idx;
  while ((idx = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, idx).trim();
    buf = buf.slice(idx + 1);
    if (!line.startsWith("data:")) continue;
    const payload = line.slice(5).trim();
    if (payload === "[DONE]") break;
    process.stdout.write(JSON.parse(payload).token);
  }
}
```

### Swift (native SDK)

Embed `libaria-agent_ffi` and the generated `AriaAgent` module (see
`bindings/swift/README.md`):

```swift
import AriaAgent

let agent = createAgent(AgentConfig(
    session: "default",
    agentName: "agent",
    sandboxProvider: "docker",
    model: "gpt-4o-mini"))
let reply = agent.run("hello")
let session = agent.session()
session.memorize(key: "fact1", value: "the moon is cheese")
print(session.recall(key: "fact1") ?? "")
```

### Kotlin (native SDK)

Add the `com.ariacompute:agent` artifact and the `libaria-agent_ffi` native
library (see `bindings/kotlin/agent-sdk/README.md`):

```kotlin
import com.ariacompute.agent.uniffi.aria_agent_ffi.*

fun main() {
    val agent = createAgent(AgentConfig("default", "agent", "docker", "gpt-4o-mini"))
    val reply = agent.run("hello")
    val session = agent.session()
    session.memorize("fact1", "the moon is cheese")
    println(session.recall("fact1"))
}
```

## License

MIT.

## Engineering Conventions

This repository follows the Harness Engineering philosophy:

- [`AGENTS.md`](AGENTS.md): Agent engineering context entry and directory index
- [`requirements.md`](requirements.md): Requirements spec (feature boundaries/exceptions/acceptance criteria, human-review-gated)
- [`task.md`](task.md): Implementation task checklist
