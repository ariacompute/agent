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

## codex 子模块

本仓库以 git 子模块方式依赖 OpenAI 的
[`codex`](https://github.com/openai/codex)，位于 `codex/`。上游 codex 的
Rust 工作区位于 `codex/codex-rs/`，**仓库根目录没有 `Cargo.toml`**，因此通过
git 依赖（`package = "codex-*"`）无法解析，必须打一个补丁。

`patch/0001-Make-codex-a-root-cargo-workspace-edition-2024.patch` 把
`codex-rs` 的工作区定义提升到仓库根目录（并把内部的 `path` 依赖统一加上
`codex-rs/` 前缀）。这正是 `ariacompute/codex` fork（branch `main`）
所做的改动。在检出后，于 **`codex/` 子模块内部**应用该补丁：

```bash
# 1. 克隆子模块（浅克隆）
git submodule update --init --depth 1 codex

# 2. 在子模块内部应用根工作区补丁
git -C codex apply ../patch/0001-Make-codex-a-root-cargo-workspace-edition-2024.patch
```

应用后，`codex/Cargo.toml` 会出现在仓库根目录，cargo 即可传递性地解析
`codex-*` 各个 crate。该补丁**不会**提交进子模块——它存放在仓库根的
`patch/` 目录——因此每次重新初始化子模块时都需重新执行。如需撤销补丁并恢复
子模块原状：

```bash
git -C codex checkout -- . && git -C codex clean -fd
```

> 该补丁是 `docs/adr/0005-codex-integration.md` 描述的深度编译集成所必需的。
> 若你只构建不依赖 codex crate 的本仓库 crate，可以跳过此步。

## 快速开始

```bash
# 1. codex 子模块 + 根工作区补丁（见上文「codex 子模块」）
git submodule update --init --depth 1 codex
git -C codex apply ../patch/0001-Make-codex-a-root-cargo-workspace-edition-2024.patch

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
#   --port <n>  监听端口（默认：CLOUD_PORT 环境变量或 3000）
aria-agent serve --port 3000
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
| `AGENT_CLOUD_API_KEY` | _(空)_ | 引导/管理员密钥。设置后作为 Admin principal（以 `Authorization: Bearer <key>` 或 `ApiKey <key>` 携带）；管理员可经 `POST /v1/api-keys` 发放租户密钥。无有效密钥的请求返回 `401`。留空则为开放模式（`default` 租户）。 |
| `AGENT_MEMO_BACKEND` | `memory` | memo 后端：`memory`（内存）或 `memo`（持久化 sled DB）。 |
| `MEMO_DIR` | `/app/.memo` | 持久化 memo 目录（仅 `AGENT_MEMO_BACKEND=memo` 时使用）。 |
| `REEF_DIR` | `/app/.reef` | Reef 存储（records / feedback / git 版本化 harness）。 |
| `RUST_LOG` | `info` | Rust 日志级别：`error` \| `warn` \| `info` \| `debug` \| `trace`。 |
| `CLOUD_PORT` | `3000` | 对 `cloud` 服务**直连**暴露的主机端口（容器内始终监听 3000）。 |
| `DOCKER_HOST` | `unix:///var/run/docker.sock` | sandbox 使用的 Docker Engine API 端点（DooD）。可改为远程 / DinD 守护进程。 |
| `ARIA_DOCKER_SOCKET` | _(未设置)_ | 显式指定 sandbox socket 路径，优先于自动探测（如 macOS 的 `~/.docker/run/docker.sock`）。 |
| `DOCKER_GID` | `998` | 宿主 `docker` 组（拥有 `/var/run/docker.sock`）的 GID。运行期镜像会重建该组并把非 root 用户 `aria` 加入，从而无需 `root` 即可访问 socket。 |
| `SANDBOX_CPUS` | `1.0` | 每个工具的 sandbox CPU 上限（如 `1.0`、`0.5`）。通过 Engine API 作用于 Docker 与 Kata。 |
| `SANDBOX_MEMORY` | `512m` | 每个工具的 sandbox 内存上限，可读单位（`512m`、`1g` …）。作用于 Docker 与 Kata；Cube 为 best-effort。 |
| `SANDBOX_PIDS_LIMIT` | `256` | sandbox 内最大进程数（pids cgroup）。作用于 Docker 与 Kata；Cube 为 best-effort。 |
| `NGINX_PORT` | `80` | `nginx` 反向代理对外暴露的主机端口（转发到 `cloud:3000`）。 |

> `.env` 已被 git 忽略；`.env.example` 是提交到仓库的模板。Compose 会自动加载
> `.env` 做变量替换，且 `cloud` 服务通过 `env_file` 直接读取它，因此容器进程能
> 拿到每一个变量。

### 2. 运行

```bash
docker compose up --build
```

云端服务会等待 Postgres 健康后再启动；表结构（`agents` / `runs`）在启动时由
`ensure_schema` 自动创建，无需初始化脚本。API 可通过**任一**入口访问：

- **直连：** `http://localhost:3000`（或 `http://localhost:${CLOUD_PORT}`）——`cloud`
  服务端口，便于调试或不需要 nginx 时使用。
- **经 nginx：** `http://localhost:80`（或 `http://localhost:${NGINX_PORT}`）——`nginx`
  反向代理，转发到 `cloud:3000`（推荐入口，会附带 `X-Forwarded-*` 头且不缓冲 SSE 流式响应）。

`nginx` 自身提供 `GET /healthz` 健康检查端点（直接由 nginx 返回 `ok`）。

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

### 4. 通过 Docker socket 的 sandbox

当 agent 调用需要隔离的工具时，云端服务通过 Docker **Engine API**（默认的
`docker` sandbox provider）拉起一个短生命周期容器来执行。该方式对齐 playground：
`cloud` 容器挂载**宿主** Docker socket（`/var/run/docker.sock`，DooD），直接访问
宿主守护进程——**镜像内不安装 `docker` CLI**。

端点由 `DOCKER_HOST` 控制（默认 `unix:///var/run/docker.sock`），因此你可以不改
代码地改为指向远程守护进程或 Docker-in-Docker 边车（如 `tcp://dind:2375`）。
工具容器运行在宿主守护进程上，命名为 `aria-sandbox-<uuid>`，会话结束时会被强制
移除。

连接在**首次 sandbox 调用时惰性建立**，因此守护进程缺失只会让工具执行报错，而不
会导致启动 panic。`DOCKER_HOST` / `ARIA_DOCKER_SOCKET` 未设置时，会按顺序探测常见
socket 路径——包括 `~/.docker/run/docker.sock`（**macOS Docker Desktop** 的默认位置，
`/var/run/docker.sock` 只有在开启 Docker Desktop 的「默认 socket」选项后才存在）、
Colima（`~/.colima/default/docker.sock`）以及 rootless Podman 路径。

`cloud` 容器以非 root 用户 `aria` 运行。为访问挂载的 socket，它会加入一个 GID 由
`DOCKER_GID`（默认 `998`）指定的 `docker` 组，该 GID 必须与宿主上拥有
`/var/run/docker.sock` 的组一致（`stat -c '%g' /var/run/docker.sock`，常为 998 或
999）。若不一致，可用 `docker compose build --build-arg DOCKER_GID=<gid>` 重新构建，
或在 compose 中回退为 `user: root`。若想彻底关闭 sandbox，只需注释掉
`/var/run/docker.sock` 的挂载卷（`DOCKER_GID` 构建参数随之失效）。

> 挂载宿主 socket 等于把宿主 Docker 的 root 级控制权交给了 cloud 容器，请仅在
> 受信任的单租户部署中使用。若需更强隔离，可将 `DOCKER_HOST` 指向独立守护进程
> （DinD / 远程）。

#### 自动探测 `DOCKER_GID`

`.env` 中的 `DOCKER_GID` 必须与宿主上拥有 socket 的组一致。可一次性读出并写回
`.env`：

```bash
DOCKER_GID=$(stat -c '%g' /var/run/docker.sock)
sed -i "s/^DOCKER_GID=.*/DOCKER_GID=$DOCKER_GID/" .env
echo "DOCKER_GID set to $DOCKER_GID"
```

随后重新构建，使运行期镜像采用新的组：
`docker compose build --build-arg DOCKER_GID=$DOCKER_GID`。

#### sandbox 资源限制

每个工具容器都受 `SANDBOX_CPUS`、`SANDBOX_MEMORY`、`SANDBOX_PIDS_LIMIT`
限制（见上表）。**Docker** 与 **Kata** provider 直接经 Engine API 下发
（`HostConfig` 的 `NanoCpus`、`Memory`、`PidsLimit`）；**Kata** 还会让容器运行在
`kata` runtime 下。**Cube** provider 则映射为 best-effort 的 `cube` CLI 参数
（`--cpus` / `--memory`），可能需要按你部署的 Cube 运行时调整。取值非法时会回退
默认值并记录告警，因此服务总能正常启动。

### 数据卷

| 卷 | 对应路径 | 用途 |
|----|----------|------|
| `pgdata` | Postgres | `agents` / `runs` 元数据。 |
| `reefdata` | `/app/.reef` | Reef 存储（始终持久化）。 |
| `memodata` | `/app/.memo` | 持久化 memo（仅 `AGENT_MEMO_BACKEND=memo` 时使用）。 |

## SDK 示例

使用 Aria Agent 有两种方式：

- **云端 API（HTTP）** —— 语言无关的 REST 端点，可在 Python、Rust、
  TypeScript 或任意 HTTP 客户端中调用。流式返回为 SSE，输出 **OpenAI
  Responses API 事件**（`response.created`、`response.output_text.delta`、
  `response.function_call_arguments.delta` 等），以 `data: [DONE]` 结束 —— 详见
  [OpenAI Agents API 兼容性](#openai-agents-api-兼容性)。
- **原生 SDK（进程内）** —— 通过 UniFFI 绑定把运行时直接嵌入 Swift / Kotlin
  应用（无服务端、无网络）。

所有云端示例均假设服务已启动（见「快速开始」）；若设置了
`AGENT_CLOUD_API_KEY`，需携带 `Authorization: Bearer <key>`（或 `ApiKey <key>`）
请求头。

在 `/v1/runs` 与 `/v1/runs/stream` 的请求体中，可用更易读的智能体名字 `agent`
代替 `agent_id`（会按当前 principal 解析该智能体）。`session` 字段也是可选的，
默认值为 `"default"`，且 `stream` 也可一并传入以兼容客户端写法。

### Python（云端 API）

```python
import os, json, requests

BASE = os.environ.get("ARIA_AGENT_BASE", "http://localhost:3000")
API_KEY = os.environ.get("AGENT_CLOUD_API_KEY")
headers = {"Authorization": f"Bearer {API_KEY}"} if API_KEY else {}

agent = requests.post(f"{BASE}/v1/agents", json={"name": "my-agent"}, headers=headers).json()
agent_name = agent["name"]

# 一次性运行
run = requests.post(
    f"{BASE}/v1/runs",
    json={"agent": agent_name, "session": "s1", "input": "hello"},
    headers=headers,
).json()
print(run["output"])

# 流式运行 —— OpenAI Responses API 事件
with requests.post(
    f"{BASE}/v1/runs/stream",
    json={"agent": agent_name, "session": "s1", "input": "tell me a joke"},
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
        if payload == "[DONE]":
            break
        event = json.loads(payload)
        if event["type"] == "response.output_text.delta":
            print(event["delta"], end="", flush=True)
        elif event["type"] == "response.completed":
            print("\n收执 ID：", event["response"]["metadata"]["reef_record_id"])
```

### Rust（云端 API）

添加 `reqwest`（开启 `json` 与 `stream` 特性）、`tokio`、`serde`、`serde_json` 与 `anyhow`：

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

    let run: Run = client.post(format!("{base}/v1/runs"))
        .headers(headers)
        .json(&serde_json::json!({"agent": agent.name, "session": "s1", "input": "hello"}))
        .send().await?.json().await?;

    println!("{}", run.output);

    // 流式运行 —— OpenAI Responses API 事件
    let mut res = client.post(format!("{base}/v1/runs/stream"))
        .headers(headers)
        .json(&serde_json::json!({"agent": agent.name, "session": "s1", "input": "tell me a joke"}))
        .send().await?;
    let mut buf = String::new();
    while let Some(chunk) = res.chunk().await? {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buf.find('\n') {
            let line = buf[..idx].trim().to_string();
            buf.drain(..=idx);
            let Some(payload) = line.strip_prefix("data:") else { continue };
            let payload = payload.trim();
            if payload == "[DONE]" { return Ok(()); }
            let event: serde_json::Value = serde_json::from_str(payload)?;
            if event["type"] == "response.output_text.delta" {
                print!("{}", event["delta"].as_str().unwrap_or_default());
            }
        }
    }
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
}).then((r) => r.json<{ name: string }>());

const run = await fetch(`${base}/v1/runs`, {
  method: "POST", headers,
  body: JSON.stringify({ agent: agent.name, session: "s1", input: "hello" }),
}).then((r) => r.json<{ output: string }>());
console.log(run.output);

// 流式运行 —— OpenAI Responses API 事件
const res = await fetch(`${base}/v1/runs/stream`, {
  method: "POST", headers,
  body: JSON.stringify({ agent: agent.name, session: "s1", input: "tell me a joke" }),
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
    const payload = line.slice(5).trim();
    if (payload === "[DONE]") break outer;
    const event = JSON.parse(payload);
    if (event.type === "response.output_text.delta") {
      process.stdout.write(event.delta);
    } else if (event.type === "response.completed") {
      console.log("\n收执 ID：", event.response.metadata.reef_record_id);
    }
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

## OpenAI Agents API 兼容性

`POST /v1/runs/stream` 采用 **OpenAI Responses API 流式协议**，因此 OpenAI SDK
与 OpenAI Agents SDK 可以零改造直接消费。每个 SSE `data:` 帧都是一个带 `type`
和单调递增 `sequence_number` 的 Responses 事件：

| 事件 | 载荷 |
|---|---|
| `response.created` | `response.id`、`response.metadata`（`agent_id`、`session`、`reef_record_id`） |
| `response.in_progress` | 阶段边界；额外带 `phase`（`recall` / `model` / `tool_exec` / `loop_guard`）与 `label` |
| `response.output_item.added` | `item` —— `message`（assistant）或 `function_call` |
| `response.function_call_arguments.delta` | `item_id`、`delta`（参数 JSON 分片） |
| `response.output_item.done` | 完成的 `item`；`function_call` 还附带执行结果 `result` |
| `response.output_text.delta` | `item_id`、`content_index`、`delta` |
| `response.output_text.done` | `item_id`、`content_index`、`text` |
| `response.completed` | `response.status = "completed"`、`response.metadata.reef_record_id` |
| `response.failed` | `response.error` |

流以 OpenAI 终止哨兵 `data: [DONE]` 结束。每次运行还会通过响应头
`x-reef-agent-record-id` 返回 Reef 收执 ID，取值与
`response.metadata.reef_record_id` 完全一致。

编排所需、但 Responses 协议未建模的字段（agentic `phase`、工具 `result`、Reef
收执 ID）都被放在这些 **OpenAI 形状帧的内部**作为额外字段，严格的 OpenAI
客户端会自动忽略，不会产生私有 `aria.*` 帧。

```python
# 用官方 OpenAI SDK 消费（无需任何 Aria 专属解析）
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

### 默认智能体

`ensure_schema` 会预置一个默认智能体 —— `id: agent-demo`、
`name: Agent Demo` —— 让全新部署（以及 Aria Playground）开箱即可运行。
已用旧默认值（`playground-demo`）预置过的部署，会在下次启动时被就地重命名。
用 `GET /v1/agents` 可列出当前密钥可见的智能体（管理员看到全部，租户只看自己
的）。

## 许可证

MIT.

## 工程约定

本仓库遵循 Harness Engineering：

- [`AGENTS.md`](AGENTS.md)：Agent 工程上下文入口与目录索引
- [`requirements.md`](requirements.md)：需求规格（功能边界 / 异常 / 验收，人审后编码）
- [`task.md`](task.md)：实施清单
