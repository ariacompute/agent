# agent

基于 OpenAI [`codex`](https://github.com/openai/codex) harness 构建的分层 agent
平台，向上提供 (a) **Rust + 云端 API** 与 (b) **Swift / Kotlin 原生 SDK**。

## 模块

| 模块 | 说明 |
|------|------|
| **codex submodule** | `openai/codex`，位于 `codex/`（harness、sandboxing、memories）。 |
| **aria-agent-memo** | 端侧/嵌入式**上下文存储**（sled），实现 `aria-agent-core::context` 契约。 |
| **aria-agent-sandbox** | 可插拔 `Sandbox`：Docker（默认）/ Kata / Cube。 |
| **aria-agent-core** | 统一的 agent 运行时：`recall → 模型 → memorize`，工具在 sandbox 中执行。 |
| **ariacompute-agent** | UniFFI `cdylib`（`libaria-agent_ffi`）：`SdkAgent` / `SdkSession` / `create_agent`。 |
| **aria-agent-cloud** | axum 服务：Postgres + pgvector（元数据**与**上下文），OpenAI beta Agents API。`/v1/agents`、`/v1/agents/sessions`、SSE `/v1/agents/sessions/{id}/events/stream`。 |
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

推荐使用上面的 SDK 消费该协议 —— 帧解析由 SDK 完成（对应
`result.final_output` / `result.finalOutput`）：

```python
from ariacompute_agent import Agent, Runner

agent = Agent(name="Agent Demo")
streamed = await Runner.run_streamed(agent, "tell me a joke")
async for event in streamed.events:
    if event["type"] == "agent.turn.output_text.delta":
        print(event["delta"], end="", flush=True)
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
| `API_KEY_CACHE_TTL_SEC` | `60` | API key 解析结果的缓存时间（秒）。 |
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

### 3. 上下文存储（PostgreSQL + pgvector）

对话/长期上下文保存在 Postgres 的 `context_fragments` 表（`vector(256)` +
HNSW 余弦索引），按 `principal_id` 分片。compose 使用
`pgvector/pgvector:pg16` 镜像；服务启动时会执行
`CREATE EXTENSION IF NOT EXISTS vector` 并校验，缺少扩展时**直接启动失败**
（不做关键词降级）。

端侧 SDK 继续使用自己的嵌入式 sled 存储（`aria-agent-memo`），不受此约束。

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
| （无额外卷） | — | 上下文与元数据都在 Postgres 中，无需本地卷。 |

## SDK 示例

以下示例**全部使用官方 SDK**，不再手写 HTTP。所有 SDK 都镜像 OpenAI Agents SDK
的用法：定义 `Agent`，然后运行。

| 语言 | 安装 | 包 / 模块 |
|---|---|---|
| Python | `pip install ariacompute-agent` | `ariacompute_agent`（镜像 `openai-agents`） |
| TypeScript / JavaScript | `npm i @ariacompute/agent` | `@ariacompute/agent`（镜像 `@openai/agents`） |
| Rust | `cargo add ariacompute-agent` | `aria_agent_ffi`（进程内 / 原生） |
| Swift | SwiftPM / CocoaPods `AriaAgent` | `bindings/swift`（进程内 / 原生） |
| Kotlin | `com.ariacompute:agent` | `bindings/kotlin`（进程内 / 原生） |

JS / Python SDK 是服务仍然暴露的 beta Agents REST API 的薄客户端 —— 协议细节见
[OpenAI Agents API 兼容性](#openai-agents-api-兼容性)；Rust / Swift / Kotlin SDK
把运行时**嵌入进程内**，上下文保存在端侧存储（无服务端、无网络）。

云端 SDK 需要服务已启动（见「快速开始」），并通过 `ARIA_AGENT_BASE_URL`
（默认 `http://localhost:3000`）与 `ARIA_AGENT_API_KEY`（当设置了
`AGENT_CLOUD_API_KEY` 时）配置连接。

每个示例都遵循相同的三步流程：

1. **一次性运行（one-shot run）** —— `run(agent, input)` / `Runner.run(agent, input)`；
2. **复用同一会话继续对话**（先用 `memorize` 写入一条长期记忆）；
3. **流式运行（streaming run）** —— `runStreamed` / `Runner.run_streamed`。

长期记忆保存在 agent 自己的上下文存储中：`memorize(key, value)` 会把一条事实以
`long_term` 上下文片段持久化，同一会话的后续轮次可以语义召回它（重启后依然有效）。
云端 SDK 的存储是 Postgres + pgvector；原生 SDK 的存储是端侧（on-device）存储。

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
)


async def main() -> None:
    session = Session.create(agent)

    # 1) 一次性运行
    first = await Runner.run(agent, "罗马帝国何时灭亡？", session=session)
    print(first.final_output)

    # 长期记忆保存在该会话的上下文存储（Postgres + pgvector）：
    # `memorize` 持久化一条事实，后续每一轮都能召回。
    session.memorize("user_name", "Ada")

    # 2) 复用同一会话继续对话 —— 回复可以同时引用前面的轮次与这条记忆
    second = await Runner.run(agent, "我叫什么名字？", session=session)
    print(second.final_output)
    print(session.recall("user_name"))  # -> "Ada"

    # 3) 流式运行
    streamed = await Runner.run_streamed(agent, "讲一个冷知识", session=session)
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
});

const session = await Session.create(agent);

// 1) 一次性运行
const first = await run(agent, "罗马帝国何时灭亡？", { session });
console.log(first.finalOutput);

// 长期记忆保存在该会话的上下文存储（Postgres + pgvector）：
// `memorize` 持久化一条事实，后续每一轮都能召回。
await session.memorize("user_name", "Ada");

// 2) 复用同一会话继续对话 —— 回复可以同时引用前面的轮次与这条记忆
const second = await run(agent, "我叫什么名字？", { session });
console.log(second.finalOutput);
console.log(await session.recall("user_name")); // -> "Ada"

// 3) 流式运行
const streamed = await runStreamed(agent, "讲一个冷知识", { session });
for await (const event of streamed.events) {
  if (event.type === "agent.turn.output_text.delta") {
    process.stdout.write(event.delta);
  }
}
console.log((await streamed.completed).finalOutput);
```

### Rust（进程内）

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
    // 无需传入会话：运行时为每个 agent 实例分配一个隔离的上下文作用域
    let agent = create_agent("History tutor".into(), "gpt-4o-mini".into())?;

    // 1) 一次性运行
    println!("{}", agent.run("罗马帝国何时灭亡？".into())?);

    // 长期记忆保存在端侧存储：`memorize` 把一条事实以 `long_term` 片段持久化，
    // 该 agent 的后续轮次可以召回它。
    let session = agent.session();
    session.memorize("user_name".into(), "Ada".into())?;

    // 2) 复用同一会话继续对话（同一 agent / 会话作用域）—— 回复可引用前面的轮次与记忆
    println!("{}", agent.run("我叫什么名字？".into())?);
    println!("{:?}", session.recall("user_name".into())?); // -> Some("Ada")

    // 3) 流式运行 —— 事件推送给监听器
    agent.run_stream("讲一个冷知识".into(), Box::new(Printer))?;
    Ok(())
}
```

### Swift（进程内）

嵌入 `libaria-agent_ffi` 与生成的 `AriaAgent` 模块（见 `bindings/swift/README.md`）。

```swift
import AriaAgent

do {
    // 无需传入会话：运行时为每个 agent 实例分配一个隔离的上下文作用域
    let agent = try createAgent(agentName: "History tutor", model: "gpt-4o-mini")

    // 1) 一次性运行
    print(try agent.run("罗马帝国何时灭亡？"))

    // 长期记忆保存在端侧存储：`memorize` 把一条事实以 `long_term` 片段持久化，
    // 该 agent 的后续轮次可以召回它。
    let session = agent.session()
    try session.memorize(key: "user_name", value: "Ada")

    // 2) 复用同一会话继续对话（同一 agent / 会话作用域）—— 回复可引用前面的轮次与记忆
    print(try agent.run("我叫什么名字？"))
    print(try session.recall(key: "user_name") ?? "")   // -> "Ada"

    // 3) 流式运行 —— 事件交给监听器处理
    try agent.runStream("讲一个冷知识", listener: Printer())
} catch {
    print(error)
}
```

### Kotlin（进程内）

添加 `com.ariacompute:agent` 依赖与 `libaria-agent_ffi` 原生库（见
`bindings/kotlin/agent-sdk/README.md`）。

```kotlin
import com.ariacompute.agent.uniffi.aria_agent_ffi.*

fun main() {
    // 无需传入会话：运行时为每个 agent 实例分配一个隔离的上下文作用域
    val agent = createAgent("History tutor", "gpt-4o-mini")

    // 1) 一次性运行
    println(agent.run("罗马帝国何时灭亡？"))

    // 长期记忆保存在端侧存储：`memorize` 把一条事实以 `long_term` 片段持久化，
    // 该 agent 的后续轮次可以召回它。
    val session = agent.session()
    session.memorize("user_name", "Ada")

    // 2) 复用同一会话继续对话（同一 agent / 会话作用域）—— 回复可引用前面的轮次与记忆
    println(agent.run("我叫什么名字？"))
    println(session.recall("user_name"))   // -> "Ada"

    // 3) 流式运行 —— 事件交给监听器处理
    agent.runStream("讲一个冷知识", Printer())
}
```

## OpenAI Agents API 兼容性

`POST /v1/agents/sessions/{id}/events/stream` 采用 **OpenAI beta Agents 流式协议**，
因此 OpenAI 客户端可以零改造直接消费。每个 SSE `data:` 帧都是 `agent.*` 事件，
带 `event_id`、`session_id`、`turn_id` 与单调递增的 `sequence_number`：

| 事件 | 载荷 |
|---|---|
| `agent.turn.created` | `turn{id,status,session_id,agent_id}` |
| `agent.turn.in_progress` | 阶段边界；额外带 `phase`（`recall` / `model` / `tool_exec` / `loop_guard`）与 `label` |
| `agent.turn.item.added` | `item` —— `message`（assistant，`role`）或 `function_call`（`name`、`arguments`） |
| `agent.turn.item.done` | 完成的 `item`；`function_call` 还附带执行结果 `result` |
| `agent.turn.output_text.delta` | `item_id`、`delta` |
| `agent.turn.output_text.done` | `item_id`、`text` |
| `agent.turn.completed` | 终止事件：`turn{status:"completed", output}` |
| `agent.turn.failed` | 终止事件：`turn{status:"failed"}`、`error` |

`agent.turn.completed`（或 `agent.turn.failed`）即为终止事件：客户端收到它便
意味着运行结束（没有额外的 `data: [DONE]` 尾帧）。

编排所需、但 beta 协议未建模的字段（agentic `phase`、工具 `result`）都被放在
这些帧**内部**作为额外字段，严格的 OpenAI 客户端会自动忽略，不会产生私有
`aria.*` 帧。

推荐使用上面的 SDK 消费该协议 —— 帧解析由 SDK 完成（对应
`result.final_output` / `result.finalOutput`）：

```python
from ariacompute_agent import Agent, Runner

agent = Agent(name="Agent Demo")
streamed = await Runner.run_streamed(agent, "tell me a joke")
async for event in streamed.events:
    if event["type"] == "agent.turn.output_text.delta":
        print(event["delta"], end="", flush=True)
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
