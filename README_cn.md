# agent

基于 OpenAI [`codex`](https://github.com/openai/codex) harness 构建的分层 agent
平台，向上提供 (a) **Rust + 云端 API** 与 (b) **Swift / Kotlin 原生 SDK**。

## 模块

| 模块 | 说明 |
|------|------|
| **codex submodule** | `openai/codex`，位于 `codex/`（harness、sandboxing、memories）。 |
| **aria-agent-memo** | 统一的**上下文记忆（memo）**。本地/嵌入式存储，**不使用 Postgres**。 |
| **aria-agent-sandbox** | 可插拔 `Sandbox`：Docker（默认）/ Kata / Cube。 |
| **aria-agent-core** | 统一的 agent 运行时：`recall → 模型 → memorize`，工具在 sandbox 中执行。 |
| **ariacompute-agent** | UniFFI `cdylib`（`libaria-agent_ffi`）：`SdkAgent` / `SdkSession` / `create_agent`。 |
| **aria-agent-cloud** | axum 服务，Postgres 仅存元数据，调用 OpenAI Agents API。`/v1/agents`、`/v1/runs`、SSE `/v1/runs/stream`。 |
| **bindings** | `bindings/swift`（SwiftPM）与 `bindings/kotlin`（Android）。 |

> **存储边界：** memo 是**唯一的**上下文存储，从不使用 Postgres；Postgres
> 仅由 `aria-agent-cloud` 用于存储 `agents`/`runs` 元数据。

## 快速开始

```bash
# 1. codex 子模块
git submodule update --init --depth 1 codex

# 2. 构建
cargo build --workspace

# 3. 测试
cargo test --workspace

# 4. lint
cargo clippy --workspace --all-targets -- -D warnings

# 5. 生成 Swift/Kotlin 绑定
# = cargo run -p aria-agent-ffigen
just ffi

# 6. 运行云端服务
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/agent
export OPENAI_API_KEY=sk-...
cargo run -p aria-agent-cloud
```

## SDK 示例

使用 Aria Agent 有两种方式：

- **云端 API（HTTP）** —— 语言无关的 REST 端点，可在 Python、Rust、
  TypeScript 或任意 HTTP 客户端中调用。流式返回为 SSE
  （`data: {"token":"..."}`，以 `data: [DONE]` 结束）。
- **原生 SDK（进程内）** —— 通过 UniFFI 绑定把运行时直接嵌入 Swift / Kotlin
  应用（无服务端、无网络）。

所有云端示例均假设服务已启动（见「快速开始」）；若设置了
`AGENT_CLOUD_API_KEY`，需携带 `Authorization: Bearer <key>`（或 `ApiKey <key>`）
请求头。

### Python（云端 API）

```python
import os, json, requests

BASE = os.environ.get("ARIA_AGENT_BASE", "http://localhost:3000")
API_KEY = os.environ.get("AGENT_CLOUD_API_KEY")
headers = {"Authorization": f"Bearer {API_KEY}"} if API_KEY else {}

agent = requests.post(f"{BASE}/v1/agents", json={"name": "my-agent"}, headers=headers).json()
agent_id = agent["id"]

# 一次性运行
run = requests.post(
    f"{BASE}/v1/runs",
    json={"agent_id": agent_id, "session": "s1", "input": "hello"},
    headers=headers,
).json()
print(run["output"])

# 流式运行
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

### Rust（云端 API）

添加 `reqwest`（开启 `json` 特性）、`tokio`、`serde`、`serde_json`：

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

> 在 Rust 中也可以直接依赖 `agent-core` / `ariacompute-agent`，在进程内运行
> 运行时，而无需经过 HTTP。

### TypeScript（云端 API）

```typescript
const base = process.env.ARIA_AGENT_BASE ?? "http://localhost:3000";
const apiKey = process.env.AGENT_CLOUD_API_KEY;
const headers: Record<string, string> = {
  "Content-Type": "application/json",
  ...(apiKey ? { Authorization: `Bearer ${apiKey}` } : {}),
};

// 创建 + 一次性运行
const agent = await fetch(`${base}/v1/agents`, {
  method: "POST", headers, body: JSON.stringify({ name: "my-agent" }),
}).then((r) => r.json<{ id: string }>());

const run = await fetch(`${base}/v1/runs`, {
  method: "POST", headers,
  body: JSON.stringify({ agent_id: agent.id, session: "s1", input: "hello" }),
}).then((r) => r.json<{ output: string }>());
console.log(run.output);

// 流式运行
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

### Swift（原生 SDK）

嵌入 `libaria-agent_ffi` 与生成的 `AriaAgent` 模块（详见
`bindings/swift/README.md`）：

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

### Kotlin（原生 SDK）

引入 `com.ariacompute:agent` 构件与 `libaria-agent_ffi` 原生库（详见
`bindings/kotlin/agent-sdk/README.md`）：

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

## 许可证

MIT.

## 工程约定

本仓库遵循 Harness Engineering：

- [`AGENTS.md`](AGENTS.md)：Agent 工程上下文入口与目录索引
- [`requirements.md`](requirements.md)：需求规格（功能边界 / 异常 / 验收，人审后编码）
- [`task.md`](task.md)：实施清单
