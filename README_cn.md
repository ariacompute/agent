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

## CLI

`aria-agent` 可执行文件（crate `aria-agent-cloud`）既是云端服务（默认子命令
`serve`），也是可自更新的命令行工具。CLI 配置存放在配套的
`~/.ariacompute/agent-cli.yml`（可用 `ARIA_COMPUTE_HOME` 覆盖主目录）。

```bash
# 交互式选择 Releases 源（默认 github，或 gitee）。
# 选择会作为 `upgrade_url` 写入 ~/.ariacompute/agent-cli.yml。
aria-agent setup
#   1) github  -> https://github.com/ariacompute
#   2) gitee   -> https://gitee.com/ariacompute

# 查看当前 CLI 配置状态。
aria-agent setup --status

# 删除 CLI 配置文件。
aria-agent setup --clear

# 从 Releases 自更新本程序与 libaria-agent_ffi。
# 读取 agent-cli.yml 中的 `upgrade_url`；用 --url 可临时覆盖。
aria-agent upgrade            # 最新稳定版
aria-agent upgrade 0.7.2      # 指定版本
aria-agent upgrade --url https://github.com/ariacompute

# 启动云端 HTTP 服务（默认子命令）。读取 agent-cli.yml，并把
# ARIA_AGENT_FFI_LIB 指向 ~/.ariacompute/lib。
aria-agent serve
```

> `serve` 子命令同样会读取 `~/.ariacompute/agent-cli.yml`，并将
> `ARIA_AGENT_FFI_LIB` 指向 `~/.ariacompute/lib`（`upgrade` 把
> `libaria-agent_ffi` 安装在此处），方便原生 SDK 找到该 cdylib。

## Docker Compose 部署

云端服务自带 `Dockerfile` 与 `docker-compose.yml`，可一键部署（Postgres +
`aria-agent` axum 服务）。所有配置都通过本地 `.env` 文件提供。

### 1. 配置

复制模板并修改取值：

```bash
cp .env.example .env
```

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `postgres` / `postgres` / `agent` | Postgres 账号与库（元数据存儲）。 |
| `DATABASE_URL` | `postgres://postgres:postgres@postgres:5432/agent` | 连接串。主机名 `postgres` 即 compose 服务名。 |
| `OPENAI_API_KEY` | _(空)_ | OpenAI 密钥，用于模型调用；仅 stub/离线可留空。 |
| `AGENT_CLOUD_API_KEY` | _(空)_ | API 鉴权开关。设置后每个请求须携带 `Authorization: Bearer <key>`（或 `ApiKey <key>`）；留空则为开放模式。 |
| `AGENT_MEMO_BACKEND` | `memory` | memo 后端：`memory`（内存）或 `memo`（持久化 sled DB）。 |
| `MEMO_DIR` | `/app/.memo` | 持久化 memo 目录（仅 `AGENT_MEMO_BACKEND=memo` 时使用）。 |
| `REEF_DIR` | `/app/.reef` | Reef 存储（records / feedback / git 版本化 harness）。 |
| `RUST_LOG` | `info` | Rust 日志级别：`error` \| `warn` \| `info` \| `debug` \| `trace`。 |
| `CLOUD_PORT` | `3000` | 对外暴露的主机端口（容器内始终监听 3000）。 |

> `.env` 已被 git 忽略；`.env.example` 是提交到仓库的模板。Compose 会自动加载
> `.env` 做变量替换，且 `cloud` 服务通过 `env_file` 直接读取它，因此容器进程能
> 拿到每一个变量。

### 2. 运行

```bash
docker compose up --build
```

云端服务会等待 Postgres 健康后再启动；表结构（`agents` / `runs`）在启动时由
`ensure_schema` 自动创建，无需初始化脚本。随后 API 在
`http://localhost:3000`（或 `http://localhost:${CLOUD_PORT}`）可用。

### 3. Memo 后端

对话上下文存储（`agent-memo`）由 `AGENT_MEMO_BACKEND` 切换：

- `memory`（默认）—— 内存 sled，重启即丢。
- `memo` —— 持久化 sled DB，目录为 `MEMO_DIR`；compose 的 `memodata` 卷使其在
  重启后保留。

```bash
# 持久化对话记忆
AGENT_MEMO_BACKEND=memo docker compose up --build
```

> Reef 数据（records / feedback / harness）无论 memo 后端如何，始终持久化到
> `reefdata` 卷。

### 数据卷

| 卷 | 对应路径 | 用途 |
|----|----------|------|
| `pgdata` | Postgres | `agents` / `runs` 元数据。 |
| `reefdata` | `/app/.reef` | Reef 存储（始终持久化）。 |
| `memodata` | `/app/.memo` | 持久化 memo（仅 `AGENT_MEMO_BACKEND=memo` 时使用）。 |

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

添加 `reqwest`（开启 `json` 特性）、`tokio`、`serde`、`serde_json` 与 `anyhow`：

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
