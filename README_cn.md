<div align="center">
<a name="readme-top"></a>

# agent

**面向 AI Agent 的分层 agent 平台**

以能力接缝（capability seams）组合的 agent 运行时，向上提供 (a)<br/>
**Rust + 云端 API**（对应 **OpenAI beta Agents** 资源面）、(b) **JS / Python<br/>
SDK**（该 HTTP API 的客户端），以及 (c) **Swift / Kotlin 原生 SDK** —— 它们共享<br/>
同一套 `recall → runtime → memorize` 外壳与可插拔 sandbox；每一轮默认由<br/>
**DeepSeek Harness** 执行。

[English](./README.md) · **简体中文** · [文档](./docs/architecture.md)

[![][release-shield]][release-link] [![][language-shield]][language-link] [![][license-shield]][license-link] [![][lastcommit-shield]][lastcommit-link]
<br/>
</div>

<details>
<summary><kbd>目录</kbd></summary>

#### 目录

**上手**

- [✨ 功能](#-功能)
- [🏗️ 架构](#-架构)
- [🧩 模块](#-模块)
- [🚀 快速开始](#-快速开始)

**命令行与部署**

- [🧰 命令行](#-命令行)
- [🛳 部署](#-部署)
- [🔨 开发](#-开发)

**架构与 SDK**

- [🐋 本轮运行时（turn runtime）](#-本轮运行时turn-runtime)
- [🌐 浏览器 sandbox](#-浏览器-sandbox)
- [🧠 记忆图与 memo 工具族](#-记忆图与-memo-工具族)
- [🔌 OpenAI Agents API 兼容性](#-openai-agents-api-兼容性)

**SDK**

- [📦 SDK 示例](#-sdk-示例)

**信任与治理**

- [🔏 信任库安全](#-信任库安全)
- [🛡️ 受控执行](#-受控执行)
- [📚 文档](#-文档)
- [📝 许可证](#-许可证)
- [🤝 工程约定](#-工程约定)

</details>

## ✨ 功能

`agent` 让每个 SDK 都遵循同样的三步形态——定义 `Agent`、运行一次，然后复用会话
继续对话并流式输出——而运行时自行决定记忆存于何处、工具在何处执行：

- 🐋 **默认由 DeepSeek Harness 执行** —— 每一轮对话跑在外部 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh --profile sdk`，stdio 上的逐行 JSON-RPC）上，turn/step 编排、模型调用与工具执行都归 dsh；进程内 Rust 循环保留为显式 `native` 回退（`aria-agent-dsh`）。
- 🧩 **Agent 外壳** —— `aria-agent-core` 负责 `recall → memorize`、提示词装配、工具管线、会话事件与冻结的流式信封，与"由谁驱动本轮"解耦。
- 🔌 **能力接缝** —— 模型、sandbox、存储、会话、工具、审批、子智能体与调度都是可替换接缝，由插件注册表在启动期装配并 fail-closed（`aria-agent-plugin`）。
- 🛠️ **统一工具管线** —— 解析 → 权限预设 → 审批 → 分发 → 结果后处理（截断 + 外置），无审批方时拒绝并回灌原因而不是放行（`aria-agent-tools`）。
- ☁️ **云端 API** —— `aria-agent-cloud` 是一个 axum 服务，讲 OpenAI **beta Agents** 资源面，Postgres + pgvector 同时保存元数据与上下文，并支持 SSE 流式。
- 📱 **端侧存储** —— `aria-agent-memo`（aria memo / SQLite）保存 local-first 的上下文副本，与云端后端通过同一个 `ContextStore` trait 互换。
- 🌐 **五语言 SDK** —— JS / Python / Rust / Swift / Kotlin 全部是云端 API 的薄客户端；不内嵌运行时，移动端只需网络与一个 API Key。
- 🧭 **可插拔 sandbox** —— 每个工具容器都受 `SANDBOX_CPUS` / `SANDBOX_MEMORY` / `SANDBOX_PIDS_LIMIT` 限制，并经由 `DOCKER_HOST` / `ARIA_DOCKER_SOCKET` 访问守护进程。
- 🛡️ **受控执行** —— 每次出站调用都经过同一条边界：判定、替换 `${credential}`、留痕；审计、计量、空闲挂起、输出投影与检查点挂在同一组接缝上，`AGENT_EGRESS_ENFORCEMENT=network` 更是直接去掉 sandbox 的默认路由，而不是指望 `HTTP_PROXY` 被遵守（`aria-agent-governance`）。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🏗️ 架构

以能力接缝组合的 agent 运行时：`aria-agent-core` 负责 agent 外壳，每项能力都是可替换的
provider；云端与原生前端共享同一套 `recall → runtime → memorize` 流程。

### 技术栈

![AGENT 技术栈架构](./arch_cn.png)

### 数据流

一轮对话依次经过召回、Prompt 组装、Token 压力检查、模型调用、工具管线与终止事件；
`SessionEvent` 是持久化与 SSE 流式输出的单一事实来源。

![AGENT 数据流](./dataflow_cn.png)

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🧩 模块

| 模块 | 说明 |
|------|------|
| **aria-agent-memo** | 端侧/嵌入式**上下文存储**（aria memo / SQLite，与 `aria-memo` CLI 同款 `memories` 表结构），实现 `aria-agent-core::context` 契约。 |
| **aria-agent-sandbox** | 可插拔 `Sandbox`：Docker（默认）/ Kata / Cube / Browser。 |
| **aria-agent-browser** | 浏览器 sandbox：六镜像 `BrowserCatalog` + `BrowserSandbox`（复用 Docker/Kata）+ 浏览器工具族。 |
| **aria-agent-core** | Agent 外壳：`recall → runtime → memorize`、接缝定义（含 `agent_loop`）、会话事件流、工具管线、提示词装配。 |
| **aria-agent-dsh** | `agent_loop` 接缝的 DeepSeek Harness 提供方：stdio JSON-RPC 传输、按会话复用的运行时池、审批/提问桥接、通知 → `AgentEvent` 映射。 |
| **aria-agent-plugin** | 插件注册表：manifest、接缝键/角色、依赖拓扑排序、环与 core 唯一性校验、冻结快照。 |
| **aria-agent-tools** | 内置工具族：fs、shell、jobs、terminal（`pty`）、skills、web（`web`）、ask_user、plan、todo、memo。 |
| **aria-agent-governance** | 出站判定与 `${credential}` 替换、JSONL 审计、用量计量、工具输出投影、加密凭据库、空闲检测、配额守卫与检查点（ADR-0021 … 0025）。 |
| **aria-agent-egress-proxy** | `network` 强制模式下的无密钥 sidecar：经 `/internal/egress/*` 回控制面，用来源 IP 解析租户，批量上报审计。 |
| **aria-agent-orchestrator** | 子智能体（进程内 fork）、智能体团队（任务 DAG）、定时任务（单飞 ticker）。 |
| **ariacompute-agent** | UniFFI `cdylib`（`libaria-agent_ffi`）：云端 API 客户端 —— `AriaAgent` / `Session` / `run` / `runStreamed` / `memorize` / `recall`。 |
| **aria-agent-cloud** | axum 服务：Postgres + pgvector（元数据**与**上下文），OpenAI beta Agents API。`/v1/agents`、`/v1/agents/sessions`、SSE `/v1/agents/sessions/{id}/events/stream`。 |
| **bindings** | `bindings/swift`（SwiftPM）与 `bindings/kotlin`（Android）。 |
| **sdk/js** | npm `@ariacompute/agent` —— beta Agents API 的 TypeScript/JavaScript 客户端（`Agent`、`run`、`runStreamed`、`tool`、`Session`）。 |
| **sdk/python** | pip `ariacompute-agent` —— beta Agents API 的 Python 客户端（`Agent`、`Runner`、`function_tool`、`Session`）。 |

> **存储边界：** **云端**把对话 / 长期上下文保存在 Postgres + pgvector
>（`context_fragments`，按 principal 分片）—— pgvector 为必需，缺少则启动失败。
> SDK 的 `local` 后端则在端侧保留一份 aria memo（SQLite）副本。详见
>`docs/adr/0010-context-storage-pgvector.md`。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🚀 快速开始

```bash
# 1. 构建
cargo build --workspace

# 2. 测试
cargo test --workspace

# 3. lint
cargo clippy --workspace --all-targets -- -D warnings

# 4. 生成 Swift/Kotlin 绑定
# = cargo run -p aria-agent-ffigen
just ffi

# 5. 本地启动 pgvector，然后用 cargo run 运行云端 API
[ -f .env ] || cp .env.example .env  # 先在 .env 中设置 GATEWAY_API_KEY
set -a
source ./.env
set +a

# 使用不冲突的宿主机端口 55432；localhost:5432 可能是普通 Postgres
docker rm -f aria-agent-pgvector >/dev/null 2>&1 || true
docker run -d --name aria-agent-pgvector \
  -e POSTGRES_USER="${POSTGRES_USER:-postgres}" \
  -e POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-postgres}" \
  -e POSTGRES_DB="${POSTGRES_DB:-agent}" \
  -p 55432:5432 \
  pgvector/pgvector:pg16
until docker exec aria-agent-pgvector pg_isready -U "${POSTGRES_USER:-postgres}" -d "${POSTGRES_DB:-agent}" >/dev/null 2>&1; do sleep 1; done

# .env 中的 DATABASE_URL 使用 compose 主机名 `postgres`；本地 cargo 改连 pgvector
export DATABASE_URL="postgres://${POSTGRES_USER:-postgres}:${POSTGRES_PASSWORD:-postgres}@localhost:55432/${POSTGRES_DB:-agent}"
cargo run -p aria-agent-cloud
```

> 每次运行都会重建该临时 pgvector 容器；表结构由 `ensure_schema` 在启动时创建。
> 若 55432 被占用，请同时修改 `-p` 与 `DATABASE_URL`。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🧰 命令行

`aria-agent` 可执行文件（crate `aria-agent-cloud`）既是云端服务（默认子命令
`serve`），也是可自更新的命令行工具。CLI 配置存放在配套的
`~/.ariacompute/agent-cli.yml`（可用 `ARIA_COMPUTE_HOME` 覆盖主目录）。

```bash
# 交互式选择 Releases 源（默认 github，或 gitee）。
# 选择会作为 `upgrade_url` 写入 ~/.ariacompute/agent-cli.yml。
aria-agent setup
# 1) github  -> https://github.com/ariacompute
# 2) gitee   -> https://gitee.com/ariacompute

# 查看当前 CLI 配置状态。
aria-agent setup --status

# 删除 CLI 配置文件。
aria-agent setup --clear

# 从 Releases 自更新本程序与 libaria-agent_ffi。
# 读取 agent-cli.yml 中的 `upgrade_url`；用 --url 可临时覆盖。
aria-agent upgrade        # 最新稳定版
aria-agent upgrade 0.7.2  # 指定版本
aria-agent upgrade --url https://github.com/ariacompute

# 启动云端 HTTP 服务（默认子命令）。读取 agent-cli.yml，并把
# ARIA_AGENT_FFI_LIB 指向 ~/.ariacompute/lib。
aria-agent serve
# --port <n>  监听端口（默认：CLOUD_PORT 环境变量或 3000）
aria-agent serve --port 3000
```

> `serve` 子命令同样会读取 `~/.ariacompute/agent-cli.yml`，并将
> `ARIA_AGENT_FFI_LIB` 指向 `~/.ariacompute/lib`（`upgrade` 把
> `libaria-agent_ffi` 安装在此处），方便原生 SDK 找到该 cdylib。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🛳 部署

云端服务自带 `Dockerfile` 与 `docker-compose.yml`，可一键部署（Postgres +
`aria-agent` axum 服务）。所有配置都通过本地 `.env` 文件提供。

### 1. 配置

复制模板并修改取值：

```bash
cp .env.example .env
```

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB` | `postgres` / `postgres` / `agent` | Postgres（pgvector）账号与库：元数据 **与** 上下文记忆。 |
| `DATABASE_URL` | `postgres://postgres:postgres@postgres:5432/agent` | 连接串。主机名 `postgres` 即 compose 服务名。 |
| `AGENT_HARNESS` | `dsh` | 本轮运行时：`dsh`（DeepSeek Harness）或 `native`（内置 Rust 循环）；未知取值启动即失败。 |
| `DSH_BIN` | `PATH` 上的 `dsh` | 运行时可执行文件（镜像内为 `/opt/dsh/bin/dsh`）。缺失即给出可操作报错，绝不降级。 |
| `DSH_HOME_ROOT` | `.aria/dsh` | 每租户/每会话 `DSH_HOME` 的根目录。 |
| `DSH_POOL_MAX` | `8` | 每租户保留的活跃运行时上限。 |
| `DSH_IDLE_TTL_SEC` | `900` | 池化运行时被回收前的空闲窗口（会话可凭 `DSH_HOME` 续用）。 |
| `DSH_THINKING_FILTER` | `enable` | `enable` 让 dsh 经本地回环代理访问网关，双向剥离 Anthropic `redacted_thinking` 块（内置 adapter 无法回放该块，第二轮会报 `UNSUPPORTED_CONTENT`），`thinking` 块保留；`disable` 直连网关。其它取值启动即失败。 |
| `GATEWAY_BASE_URL` / `GATEWAY_API_KEY` / `GATEWAY_MODEL` | `https://gateway.ariacompute.com` / _(空)_ / `ariacompute/ariamodel` | **两种运行时（`dsh` 与 `native`）共用**的模型网关配置。该值是网关根地址；`native` 追加 `/v1/chat/completions`，`dsh` 追加 `/anthropic/v1/messages`。不再回退 `OPENAI_*`。`dsh` 通过子进程白名单接收这些值。仅 stub/离线可留空。 |
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
| `ARIACOMPUTE_BROWSER_IMAGE_<KIND>` | _(未设置)_ | 浏览器 sandbox 按引擎的镜像覆盖；`<KIND>` ∈ `SERVO` / `OBSCURA` / `CHROMIUM` / `GOSUB` / `CAMOUFOX` / `LIGHTPANDA`（如 `ARIACOMPUTE_BROWSER_IMAGE_CHROMIUM=my/playwright:latest`）。缺省回退内置默认值。 |

> `.env` 已被 git 忽略；`.env.example` 是提交到仓库的模板。Compose 会自动加载
> `.env` 做变量替换，且 `cloud` 服务通过 `env_file` 直接读取它，因此容器进程能
> 拿到每一个变量。

> **客户端密钥一致性。** 每个客户端（例如用 `-PagentCloudApiKey=...` 构建的 cockpit
> Android App）会把 `AGENT_CLOUD_API_KEY`（或经 `POST /v1/api-keys` 发放的密钥）原样
> 作为 `Authorization: Bearer <key>` 发送。该密钥必须是**本服务端有效**的密钥，**或者**
> 当本项为**空**（开放模式）时，客户端也**完全不发送**密钥。服务端不认识的密钥（例如
> `ak_live_x` 这类占位值）会返回 `401`。请保证本服务端密钥与所有客户端的密钥
> **保持一致（同一有效密钥）或同时留空。**

### 2. 构建

`cloud` 服务使用 `aria-agent-cloud` 镜像。你可以手动构建，也可以让 compose 帮你
构建——两种方式生成的是同一个镜像。

**方式 A — 手动构建，再 `up`（复用已构建镜像）：**

```bash
docker build \
  --network=host \
  --build-arg BUILD_PROXY=http://127.0.0.1:7897 \
  --build-arg DOCKER_GID=$(stat -c '%g' /var/run/docker.sock) \
  -f Dockerfile -t aria-agent-cloud .
docker compose up
```

**方式 B — 一条命令由 compose 构建并运行：**

```bash
export BUILD_PROXY=http://127.0.0.1:7897  # 可选；不设置则直连公网
export DOCKER_GID=$(stat -c '%g' /var/run/docker.sock)
docker compose up --build
```

构建参数（均为可选）：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `BUILD_PROXY` | _(空)_ | 构建期内 `apt`/`cargo`/`git` 使用的 HTTP(S) 代理。留空 ⇒ 直连公网。配合 `--network=host`（或 compose 的 `build.network: host`），使构建容器内 `127.0.0.1` 即宿主机侧代理（如 `127.0.0.1:7897` 的 Clash）。 |
| `DOCKER_GID` | `998` | 拥有 `/var/run/docker.sock` 的宿主机 `docker` 组 GID；非 root 的 `aria` 用户加入该组后即可访问挂载的 socket。 |
| `APT_MIRROR` | `mirrors.aliyun.com` | apt 镜像源 *主机名*（如 `mirrors.tuna.tsinghua.edu.cn`、`mirrors.ustc.edu.cn`）。 |

> 构建使用 `network: host` 是 **Linux** 下访问 `127.0.0.1` 代理的要求。在 Docker
> Desktop（Mac/Windows）上构建阶段不支持 host 网络——请将代理监听在 `0.0.0.0`，
> 或不加 `network: host` 而改用可路由的宿主机地址作为代理。

### 3. 运行

```bash
docker compose up --build
```

#### API 冒烟测试

服务起来之后，可运行一次性 HTTP 冒烟测试（**不构建、不启动、不停止**，只探测已运行实例）。它加载与 `docker compose up` **同一份** `.env`（缺失时回退到 `.env.example`）以读取 `AGENT_CLOUD_API_KEY`，而 `BASE_URL` 通过命令行参数或环境变量传入：

```bash
bash scripts/test-api.sh --base-url http://localhost:3000
# 或：
BASE_URL=http://localhost:3000 bash scripts/test-api.sh
```

脚本会对每条路由（agents、sessions、memory、relations、subagents、api-keys、模型驱动的 `/events` 与 `/events/stream`，以及已识别但未实现的 501 stub）断言状态码与关键 JSON 字段，打印 `TOTAL / PASS / FAIL / SKIP`，并以失败数作为退出码。单 agent 运行时的 subagent 子资源路由按 `404` 断言，不再跳过。`/events` 与 `/events/stream` 默认执行（需已配置模型网关）；`--events-test` / `EVENTS_TEST` 仅为兼容旧调用的空操作。

云端服务会等待 Postgres 健康后再启动；表结构（`agents` / `runs`）在启动时由
`ensure_schema` 自动创建，无需初始化脚本。API 可通过**任一**入口访问：

- **直连：** `http://localhost:3000`（或 `http://localhost:${CLOUD_PORT}`）——`cloud`
  服务端口，便于调试或不需要 nginx 时使用。
- **经 nginx：** `http://localhost:80`（或 `http://localhost:${NGINX_PORT}`）——`nginx`
  反向代理，转发到 `cloud:3000`（推荐入口，会附带 `X-Forwarded-*` 头且不缓冲 SSE 流式响应）。

`GET /healthz` 是存活探针：由 `cloud` 服务自身提供（无需 API key），`nginx` 将 `/healthz`
转发到上游，因此结果反映真实后端健康状态。`cloud` 的 compose 健康检查执行
`curl -fsS http://localhost:3000/healthz`，`nginx` 仅在 `cloud` 健康后才启动。

### 4. 上下文存储（PostgreSQL + pgvector）

对话/长期上下文保存在 Postgres 的 `context_fragments` 表（`vector(256)` +
HNSW 余弦索引），按 `principal_id` 分片。compose 使用
`pgvector/pgvector:pg16` 镜像；服务启动时会执行
`CREATE EXTENSION IF NOT EXISTS vector` 并校验，缺少扩展时**直接启动失败**
（不做关键词降级）。

SDK 的 `local` 后端通过 `aria-memo` CLI 在端侧保留 aria memo（SQLite）副本，不受此约束。

### 5. 通过 Docker socket 的 sandbox

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

### 6. 数据卷

| 卷 | 对应路径 | 用途 |
|----|----------|------|
| `pgdata` | Postgres（pgvector） | `agents` / `runs` 元数据 **与** `context_fragments`（上下文记忆）。 |

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🔨 开发

| 目标 | 作用 |
|---|---|
| `cargo build --workspace` | 构建全部 crate |
| `cargo test --workspace` | 运行 workspace 测试套件 |
| `just e2e` | 运行真实 socket 的 HTTP E2E 套件（可选 DB/Docker 场景缺依赖时跳过） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 以警告即错误方式 lint |
| `just ffi` | 重新生成 Swift / Kotlin 绑定（需先构建 cdylib） |
| `cargo run -p aria-agent-cloud` | 运行云端 API（需 Postgres + OpenAI 密钥） |
| `aria-agent serve` | 启动云端 HTTP 服务（默认子命令） |
| `aria-agent setup` / `upgrade` | 配置 Releases 源 / 自更新二进制与 cdylib |

### E2E 测试

所有与 E2E 相关的 `just` 命令：

| 命令 | 作用 |
|---|---|
| `just e2e` | 运行真实 socket 的 HTTP E2E 套件（`cargo test -p agent-tests --test e2e_http`）；可选 DB/Docker 场景缺依赖时跳过。 |
| `just dsh-smoke` | 拉起脚本化假 `dsh`，校验 JSON-RPC 握手与回合帧序（`cargo test -p aria-agent-dsh --test stdio -- --nocapture`）；离线、无需凭据。 |
| `just dsh-e2e` | 用真实 `dsh` 跑一轮（`ARIA_DSH_E2E=1 cargo test -p aria-agent-dsh --test e2e_live -- --nocapture`）；需本地 `dsh` 以及 `GATEWAY_BASE_URL` / `GATEWAY_API_KEY`（可选 `GATEWAY_MODEL`）。该 `just` 任务只设置 `ARIA_DSH_E2E=1`；`just` 默认不加载 `.env`，请用下面两种方式之一加载。 |

**本地安装 `dsh`。** 镜像通过 `deepseek-harness-sdk`（它会拉
`deepseek-harness-runtime-bin`）把 `dsh` 装到 `/opt/dsh/bin/dsh`。本地同样安装：

```bash
cd <仓库根目录>
python3 -m venv .venv-dsh
.venv-dsh/bin/pip install --upgrade pip
.venv-dsh/bin/pip install deepseek-harness-sdk
test -x .venv-dsh/bin/dsh && echo "dsh ready"
```

`.env` 默认写的是镜像路径 `DSH_BIN=/opt/dsh/bin/dsh`。用上面的 venv 二进制覆盖它
（命令行临时覆盖，或直接修改 `.env`），然后用以下两种方式之一把 `.env` 加载进测试进程：

```bash
# shell source：DSH_BIN 覆盖要放在 source 之后
set -a; . ./.env; set +a; DSH_BIN="$PWD/.venv-dsh/bin/dsh" just dsh-e2e

# 或让 just 加载 .env：shell 中显式指定的 DSH_BIN 优先
DSH_BIN="$PWD/.venv-dsh/bin/dsh" just --dotenv-path .env dsh-e2e
```

`set -a` 会把 `.env` 中的赋值导出，使 `cargo test` 子进程能读到
`GATEWAY_API_KEY`；`--dotenv-path` 则让 `just` 把同一文件加载进 recipe 环境。
若两者都不用，测试会打印 `skip: GATEWAY_API_KEY is not set` 并返回 `ok` ——
这是跳过，不是通过。

`just e2e` 将生产 axum router 绑定到临时回环端口，用真实 HTTP 客户端驱动 beta Agents API，覆盖 agent/session/turn 生命周期、SSE 帧、工具调用、租户隔离、记忆与异常路径。两个可选场景在缺少依赖时打印跳过原因，不会伪造通过：

- `DATABASE_URL` —— 通过两个独立构建的 router 验证 Postgres + pgvector 存储；未设置或不可达时跳过。
- `ARIA_E2E_DOCKER=1` —— 经流式路由在真实容器中执行 `shell` 工具；需 Docker 守护进程可达且本地已有 `alpine:latest` 镜像，否则跳过。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🐋 本轮运行时（turn runtime）

turn 循环本身就是 `agent_loop` 接缝，默认提供方是 **DeepSeek Harness**：每个会话一个
`dsh` 子进程，经 JSON-RPC 通信，拥有独立的 `DSH_HOME` 与工作目录、白名单环境变量，并以
池化方式复用（空闲回收）。dsh 同时执行模型调用**与它自己的工具**，所以在
`harness = "dsh"` 下 aria 的 sandbox/工具管线不再参与本轮；aria 仍负责 recall、记忆落库、
会话事件与标题、审批/提问桥接以及对外信封。`AGENT_HARNESS=native` 可显式切回
内置 Rust 循环；未知取值，或选了 `dsh` 却没有可用运行时，都直接报错，绝不静默降级。

默认（`DSH_THINKING_FILTER=enable`）dsh 经本地回环过滤代理访问网关：内置
`deepseek-official` adapter 首轮接受 Anthropic `redacted_thinking` 块并持久化，第二轮回放
历史时抛 `UNSUPPORTED_CONTENT`；代理在请求体与 JSON/SSE 响应两侧剥离该块（并重排
content-block 序号），dsh 因此永远看不到它。`thinking` 块**刻意保留**——DeepSeek 的
Anthropic 兼容层要求 thinking 模式下原样回传，剥离会把该错误换成 400。代价是思考内容
仅作展示、从不回放（与 OpenCode 同样的取舍）。只有确认所用 dsh build 的 adapter 能处理
redacted thinking 时，才应设为 `DSH_THINKING_FILTER=disable`。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🌐 浏览器 sandbox

`aria-agent-browser` 把一台真实的、隔离的浏览器嵌入到 agent 运行时中。它与命令
执行复用同一套 `Sandbox` trait 与 Docker/Kata 容器路径，因此每个浏览器容器同样
受 `SANDBOX_CPUS`、`SANDBOX_MEMORY`、`SANDBOX_PIDS_LIMIT` 限制，并经由
`DOCKER_HOST` / `ARIA_DOCKER_SOCKET` 访问守护进程，与 shell 工具完全一致。

`BrowserCatalog` 登记了 **六** 种浏览器镜像，每种各自作为一个独立的 sandbox 容器
启动：

| Kind | 引擎 | 说明 |
|------|------|------|
| `chromium` | Playwright + Chromium | 默认。完整自动化：navigate / extract / click / fill / screenshot / evaluate，并集成 `playwright-captcha` 处理验证码。 |
| `camoufox` | Camoufox（基于 Firefox） | 反检测 / 反爬画像；Playwright 兼容。 |
| `lightpanda` | Lightpanda | 面向 AI 的原生无头浏览器；轻量抽取。 |
| `obscura` | Chromium 隐身层 | 在 Chromium 之上叠加指纹对抗 / 反机器人层（`obscura`）。 |
| `servo` | Servo（Rust） | 原生引擎；仅 navigate + extract，其余操作降级为 `Unsupported`。 |
| `gosub` | Gosub engine（Rust） | 原生引擎；仅 navigate + extract，其余操作降级为 `Unsupported`。 |

用 `sandbox_provider = "browser"`（默认 kind `chromium`）开启。可在启动时选择
引擎，**也可在运行时切换**——`browser_use` 工具会销毁当前容器、按 catalog 拉起新
镜像；若镜像不可用则安全回退（不 panic、不掉线）：

```json
{ "name": "browser_use", "arguments": { "engine": "camoufox" } }
```

浏览器能力以工具族形式暴露，并优先经 `ToolHandler` 分派（未命中再回退 shell）：
`browser_navigate`、`browser_extract`、`browser_click`、`browser_fill`、
`browser_screenshot`、`browser_evaluate`、`browser_solve_captcha`，以及用于切换的
`browser_use`。示例工具调用：

```json
{ "name": "browser_navigate", "arguments": { "url": "https://example.com" } }
{ "name": "browser_extract",   "arguments": { "mode": "markdown" } }
```

镜像标签可按引擎经 `ARIACOMPUTE_BROWSER_IMAGE_<KIND>`（如
`ARIACOMPUTE_BROWSER_IMAGE_CHROMIUM`）覆盖，缺省回退内置默认值。详见
`docs/adr/0012-browser-sandbox.md`。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🧠 记忆图与 memo 工具族

agent 不只是读/写 `aria-agent-memo` 存储——它会在**运行过程中主动管理自己的记忆**：

* **记忆图（memory graph）。** 每一轮在向量 `recall` 之外，用受界的图 `expand`
  （以召回片段为种子）补充上下文，并把该轮 user↔assistant 片段用 `Temporal`
  关系相连；后续轮次可沿对话图检索上下文，而不仅依赖嵌入相似度。
* **自动压缩（auto-compact）。** 当某会话片段数超过配置阈值（SDK 默认 `32`）时，
  运行循环将其 `compact` 为单条笔记（幂等——多次压缩复用同一笔记 id，历史有界）。
* **memo 工具族。** 模型可调用四个长期记忆工具，经 `ToolHandler` 在 shell 回退之前
  分派：

  | 工具 | 用途 |
  |------|------|
  | `memo_store` | 按 key 持久化一条长期记忆（如用户偏好）。 |
  | `memo_get` | 按 key 读回一条长期记忆。 |
  | `memo_search` | 对会话记忆做语义（向量）检索。 |
  | `memo_related` | 检视记忆图：列出 `from`/`to`/`kind`/`score` 各边。 |

记忆图与自动压缩都由 `AgentConfig::memory_graph` 控制（在 `aria-agent-core` 中默认
关闭以保证其单测封闭；SDK 默认开启）。`aria-agent-core` 只依赖 `ContextStore` trait、
绝不依赖 `aria-agent-memo`，因此本地（aria memo / SQLite）与云端（pgvector）后端可互换。
详见 `docs/adr/0013-agent-memo-integration.md`。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🔌 OpenAI Agents API 兼容性

`POST /v1/agents/sessions/{id}/events/stream` 采用 **OpenAI beta Agents 流式协议**，
因此 OpenAI 客户端可以零改造直接消费。每个 SSE `data:` 帧都是 `agent.*` 事件，
带 `event_id`、`session_id`、`turn_id` 与单调递增的 `sequence_number`：

| 事件 | 载荷 |
|---|---|
| `agent.turn.created` | `turn{id,status,session_id,agent_id}` |
| `agent.turn.in_progress` | 阶段边界；额外带 `phase`（`recall` / `harness` / `step` / `model` / `tool_exec` / `loop_guard`）与 `label` |
| `agent.turn.item.added` | `item` —— `message`（assistant，`role`）或 `function_call`（`name`、`arguments`） |
| `agent.turn.item.done` | 完成的 `item`；`function_call` 还附带执行结果 `result` |
| `agent.turn.output_text.delta` | `item_id`、`delta` |
| `agent.turn.output_text.done` | `item_id`、`text` |
| `agent.turn.approval.requested` | `approval_id`、`tool`、`item_id` |
| `agent.turn.approval.resolved` | `approval_id`、`tool`、`decision`（`allow` / `deny` / `unavailable`）、`reason` |
| `agent.turn.usage` | `usage{input_tokens, output_tokens, total_tokens}`、`model` |
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
    elif event["type"] == "agent.turn.approval.resolved":
        print(f"\n[tool {event['tool']}: {event['decision']}]")
    elif event["type"] == "agent.turn.usage":
        print(f"\n[tokens: {event['usage']['total_tokens']}]")
```

### 默认智能体

`ensure_schema` 会预置一个默认智能体 —— `id: agent-demo`、
`name: Agent Demo` —— 让全新部署（以及 Aria Playground）开箱即可运行。
已用旧默认值（`playground-demo`）预置过的部署，会在下次启动时被就地重命名。
用 `GET /v1/agents` 可列出当前密钥可见的智能体（管理员看到全部，租户只看自己
的）。SDK 引用不存在的智能体 **名称** 时，会在首次创建会话时自动建号；显式传入
不存在的 `agent_id` 仍返回 404，不会静默新建。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 📦 SDK 示例

以下示例**全部使用官方 SDK**，不再手写 HTTP。所有 SDK 都遵循同样的用法：
定义 `Agent`，然后运行。

| 语言 | 安装 | 包 / 模块 |
|---|---|---|
| Python | `pip install ariacompute-agent` | `ariacompute_agent`（云端 API 客户端） |
| TypeScript / JavaScript | `npm i @ariacompute/agent` | `@ariacompute/agent`（云端 API 客户端） |
| Rust | `cargo add ariacompute-agent` | `aria_agent_ffi`（云端 API 客户端） |
| Swift | SwiftPM / CocoaPods `AriaComputeAgent` | `bindings/swift`（云端 API 客户端） |
| Kotlin | `com.ariacompute:agent` | `bindings/kotlin`（云端 API 客户端） |

所有 SDK 都是服务暴露的 beta Agents REST API 的薄客户端 —— 协议细节见
[OpenAI Agents API 兼容性](#-openai-agents-api-兼容性)。Rust / Swift / Kotlin 通过
UniFFI 使用同一套 HTTP 接口：设备上没有内嵌运行时、没有 sandbox、也不需要模型密钥；
唯一的本地产物是 `backend = "local"` 时的那份 aria memo 副本。

云端 SDK 需要服务已启动（见「快速开始」），并通过 `ARIA_AGENT_BASE_URL`
（默认 `http://localhost:3000`）与 `ARIA_AGENT_API_KEY`（当设置了
`AGENT_CLOUD_API_KEY` 时）配置连接。

每个示例都遵循相同的三步流程：

1. **一次性运行（one-shot run）** —— `run(agent, input)` / `Runner.run(agent, input)`；
2. **复用同一会话继续对话**（先用 `memorize` 写入一条长期记忆）；
3. **流式运行（streaming run）** —— `runStreamed` / `Runner.run_streamed`。

长期记忆保存在会话的上下文存储中：`memorize(key, value)` 会把一条事实以
`long_term` 上下文片段持久化，同一会话的后续轮次可以语义召回它（重启后依然有效）。
所有 SDK 共用同样的三种后端（见下）：`cloud` 是服务的 Postgres + pgvector 存储，
`local` 是经 `aria-memo` CLI 读写的 aria memo（SQLite）存储；Rust / Swift / Kotlin
同样不再内嵌任何存储。

### 记忆后端：cloud / local / both

每个 SDK 都可以选择记忆上下文存放在哪里：

| 后端 | 存储 | 说明 |
|---|---|---|
| `cloud` | agent-cloud（Postgres + pgvector） | 跨设备共享；需要服务在运行 |
| `local` | **aria memo**（SQLite，`memo.db`） | 端侧、可离线；可用 `aria-memo list --json` 查看 |
| `both` | 云端 **与** 本地 | 双写；读取合并去重（云端副本优先） |

既可以在 agent / session 构造时配置，也可以单次调用覆盖：

```python
agent = Agent(name="History tutor", memory_backend="both", memo_db="~/.ariacompute/memo.db")
session.memorize("user_name", "Ada")          # -> 写入 both
session.recall("user_name", backend="local")  # -> 只读 local 副本
```

```typescript
const agent = new Agent({
  name: "History tutor",
  memory: { backend: "both", memoDb: "~/.ariacompute/memo.db" },
});
await session.memorize("user_name", "Ada");               // -> 写入 both
await session.recall("user_name", { backend: "local" });  // -> 只读 local 副本
```

`both` 不伪造数据：单端失败只记录告警并容忍，双端都失败才报错。

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
    # 记忆后端：cloud | local（aria memo）| both
    memory_backend="both",
    memo_db="~/.ariacompute/memo.db",
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
    print(session.recall("user_name"))                   # -> "Ada"
    print(session.recall("user_name", backend="local"))  # 只读 local（aria memo）副本

    # 3) 流式运行
    streamed = await Runner.run_streamed(agent, "讲一个冷知识", session=session)
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

### 错误处理与限制

传输层会把失败统一归一化为带类型的 `AriaError`（`kind ∈ {auth, network, api,
config, unknown}`）：`401/403` → `auth`，其他非 2xx → `api`（携带 HTTP
`status`），连接失败 → `network`，本地误用（例如非正的 `max_turns`、未知的记忆后端）
→ `config`。一个 `agent.*` 帧都没有的流属于 `unknown` —— 绝不当成空答案。
请基于 `err.kind` 分支，而不是去匹配错误文案。

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

只读的会话端点（不修改上下文）同样可用：`session.get_items()`、
`session.get_turns()` 与 `session.get_title()`（GET `…/items`、`…/turns`、
`…/title`）。标题来自 `title_set` 事件的投影；被委派的子会话
（`<parent>::sub::<id>`）可通过父会话归属读取，尚无标题的会话返回 404。
JS SDK 镜像了上述全部能力（`AriaError`、`run`/`runStreamed` 的 `maxTurns`、
`Session.getItems()`/`getTurns()`/`getTitle()`）。

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
  // 记忆后端：cloud | local（aria memo）| both
  memory: { backend: "both", memoDb: "~/.ariacompute/memo.db" },
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
console.log(await session.recall("user_name"));                        // -> "Ada"
console.log(await session.recall("user_name", { backend: "local" }));  // 只读 aria memo

// 3) 流式运行
const streamed = await runStreamed(agent, "讲一个冷知识", { session });
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

### Rust（cloud 客户端）

Rust 侧与 JS/Python 是同一套 HTTP 客户端：没有运行时、没有 sandbox、没有模型密钥，
只有云端 API。`run` 在未传会话时自动创建，`memorize`/`recall` 可选
`cloud` | `local`（aria-memo CLI）| `both`，`run_streamed` 把每个 `agent.*` 帧交给监听器。

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
        // cloud | local（aria-memo CLI）| both；单次调用可用 backend 覆盖。
        memory: Some(MemoryConfig {
            backend: Some("both".into()),
            memo_bin: None,
            memo_db: Some("~/.ariacompute/memo.db".into()),
        }),
    })?;

    // 1) 一次性运行 —— 云端会创建并返回会话。
    let first = agent.run("When did the Roman Empire fall?".into(), None)?;
    println!("{}", first.final_output);

    // 2) 复用会话，后续回合能看到前面的对话。
    let session = agent.create_session()?;
    session.memorize("user_name".into(), "Ada".into(), None)?;
    println!("{}", agent.run("What is my name?".into(), Some(session.clone()))?.final_output);
    println!("{:?}", session.recall("user_name".into(), None)?);  // -> Some("Ada")
    println!("{:?}", session.recall("user_name".into(), Some("local".into()))?);

    // 3) 流式运行 —— 每个帧按线序交给监听器。
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

### Swift（cloud 客户端）

嵌入 `libaria-agent_ffi` 与生成的 `AriaComputeAgent` 模块（见
`bindings/swift/README.md`）。全部走 HTTP，因此 iOS 上同样可用。

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
        // cloud | local（aria-memo CLI）| both。
        memory: MemoryConfig(
            backend: "both",
            memoBin: nil,
            memoDb: "~/.ariacompute/memo.db"
        )
    ))

    // 1) 一次性运行 —— 云端会创建并返回会话。
    print(try agent.run(input: "When did the Roman Empire fall?", session: nil).finalOutput)

    // 2) 复用会话，后续回合能看到前面的对话。
    let session = try agent.createSession()
    try session.memorize(key: "user_name", value: "Ada", backend: nil)
    print(try agent.run(input: "What is my name?", session: session).finalOutput)
    print(try session.recall(key: "user_name", backend: nil) ?? "")  // -> "Ada"
    print(try session.recall(key: "user_name", backend: "local") ?? "")

    // 3) 流式运行 —— 每个帧按线序交给监听器。
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

### Kotlin（cloud 客户端）

引入 `com.ariacompute:agent` 构件与 `libaria-agent_ffi` 原生库（见
`bindings/kotlin/agent-sdk/README.md`）。全部走 HTTP，因此 Android 上同样可用。

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
            // cloud | local（aria-memo CLI）| both。
            memory = MemoryConfig(
                backend = "both",
                memoBin = null,
                memoDb = "~/.ariacompute/memo.db"
            )
        )
    )

    // 1) 一次性运行 —— 云端会创建并返回会话。
    println(agent.run("When did the Roman Empire fall?", null).finalOutput)

    // 2) 复用会话，后续回合能看到前面的对话。
    val session = agent.createSession()
    session.memorize("user_name", "Ada", null)
    println(agent.run("What is my name?", session).finalOutput)
    println(session.recall("user_name", null))  // -> "Ada"
    println(session.recall("user_name", "local"))

    // 3) 流式运行 —— 每个帧按线序交给监听器。
    val result = agent.runStreamed("Tell me something surprising", session, Printer(), null)
    println(result.finalOutput)
}
```

#### 编译 Android AAR

`bindings/kotlin/agent-sdk` 中的 Kotlin 绑定以 Android library AAR 形式发布，
其中打包了 `arm64-v8a` 与 `x86_64` 两种 ABI 的原生库 `libaria_agent_ffi.so`。
从源码编译步骤：

1. 前置条件：Android SDK + NDK（设置 `ANDROID_NDK`），以及 Rust 的 Android 目标：
   ```bash
   rustup target add aarch64-linux-android x86_64-linux-android
   ```
   新增 `agent/.cargo/config.toml`，让这两个目标使用 NDK 的 clang 链接器（API 24），例如：
   ```toml
   [target.aarch64-linux-android]
   linker = "aarch64-linux-android24-clang"
   [target.x86_64-linux-android]
   linker = "x86_64-linux-android24-clang"
   ```
   （构建时须将 NDK 的 `toolchains/llvm/prebuilt/linux-x86_64/bin` 加入 `PATH`。）
2. 为每个 ABI 编译并放置原生 cdylib：
   ```bash
   cargo build -p ariacompute-agent --target aarch64-linux-android --release
   cargo build -p ariacompute-agent --target x86_64-linux-android --release
   cp target/aarch64-linux-android/release/libaria_agent_ffi.so \
      bindings/kotlin/agent-sdk/src/main/jniLibs/arm64-v8a/
   cp target/x86_64-linux-android/release/libaria_agent_ffi.so \
      bindings/kotlin/agent-sdk/src/main/jniLibs/x86_64/
   ```
   （仅当 FFI 接口发生变化时，才需要用 `just ffi` 重新生成 Kotlin 绑定。）
3. 安装 JDK 17 与 Gradle 8.9+（本项目不含 Gradle wrapper，因此 `java` 与
   `gradle` 都必须在 `PATH` 中）。若提示 `JAVA_HOME is not set and no 'java'
   command could be found`，请安装 JDK 并导出 `JAVA_HOME`：
   ```bash
   # Ubuntu/Debian：OpenJDK 17（AGP 8.5 要求 JDK 17）
   sudo apt install openjdk-17-jdk
   export JAVA_HOME=$(dirname $(dirname $(readlink -f $(which java))))
   # 持久化（例如将上述 export 写入 ~/.bashrc）
   ```
   随后安装 Gradle 8.9+——注意 apt 源中的版本是旧的 4.x，**无法使用**：
   ```bash
   # Ubuntu/Debian：snap 渠道提供较新的 8.x（如 8.14.4）
   sudo snap install gradle  # 若受 confinement 限制可加 --classic
   # 备选 —— 官方压缩包（固定任意 8.9+ 版本）：
   #   curl -L https://services.gradle.org/distributions/gradle-8.9-bin.zip -o /tmp/gradle.zip
   #   unzip /tmp/gradle.zip -d /opt && export PATH="/opt/gradle-8.9/bin:$PATH"
   # 备选 —— SDKMAN：
   #   sdk install gradle 8.9
   ```
   （**不要**执行 `sudo apt install gradle`，它提供的是 Gradle 4.4.1，对当前 AGP/Kotlin
   插件版本而言过旧。）
4. 构建 AAR：
   ```bash
   cd bindings/kotlin/agent-sdk
   gradle assembleRelease
   ```
   产物为 `bindings/kotlin/agent-sdk/build/outputs/aar/agent-sdk-release.aar`。
   AAR 版本跟随 agent 的 git tag（如 `v1.2.0` → `1.2.0`），可用 `ARIA_VERSION`
   环境变量覆盖。

消费方可通过 `flatDir` 仓库或 Maven 仓库解析该 AAR（示例见 `cockpit/app` README）。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🔏 信任库安全

2026-10 的 eMudhra（emSign）中间 CA 私钥泄露事件，对 agent 通过两层兜底：

- **Rust（云端 + CLI 二进制）。** 出向 TLS（provider API 调用、`upgrade` 从 GitHub/Gitee
  Releases 下载）经 `reqwest` + `rustls-tls`，其信任库为编译进二进制的 **`webpki-roots`**，
  在 `aria-agent-cloud` 中钉 `>=1.0.10`，从而移除被泄露的 emSign 根。二进制不走系统
  bundle，因此锁升级是决定性修复。
- **JS SDK。** 构建与测试使用 **bun >= 1.4.4**；除对云端服务发起的 fetch 外，自身不做出向 TLS。
- **Python SDK。** 不打包信任库，依赖宿主 / 容器 `ca-certificates`。

`scripts/check-emudhra.sh` 离线扫描系统或镜像 CA bundle 中已知根/中间证书的 SHA-256
指纹（退出码 `0` 干净、`1` 命中、`2` 无法读取）：

```bash
scripts/check-emudhra.sh /etc/ssl/certs/ca-certificates.crt
```

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🛡️ 受控执行

每个出站请求都经过同一条边界：判定、替换、记录（ADR-0025）。默认值一律收紧，且只来自
`AgentConfig`，因此 CLI 与云端部署不会对「默认是什么」产生分歧。

| 变量 | 默认值 | 含义 |
|---|---|---|
| `AGENT_EGRESS_MODE` | `deny_all` | `deny_all` \| `allowlist` \| `denylist` |
| `AGENT_EGRESS_ALLOW` | *（空）* | 模式为 `allowlist` 时可访问的主机（`*.` 表示子域） |
| `AGENT_EGRESS_DENY` | *（空）* | 始终拒绝的主机 |
| `AGENT_EGRESS_ENFORCEMENT` | `proxy` | `proxy` \| `network` \| `off`（需 `AGENT_ALLOW_UNENFORCED_EGRESS=1`） |
| `AGENT_AUDIT_DIR` | `<workspace>/.aria/audit` | JSONL sink；`/v1/governance/audit/export` 返回 NDJSON |
| `AGENT_AUDIT_RETENTION_DAYS` | `30` | `0` 表示永久保留 |
| `AGENT_METERING` | `on` | 按生命周期状态计量的用量账本 |
| `AGENT_COMPRESSION` | `on` | 面向模型的规则化工具输出投影 |
| `AGENT_CREDENTIALS_DIR` | *（未设置即关闭）* | 落盘加密的凭据库 |
| `AGENT_CREDENTIAL_MASTER_KEY` | *（开启凭据库时必填）* | CHACHA20-POLY1305 的口令 |
| `AGENT_IDLE_TIMEOUT_SECS` | `900` | `0` 关闭挂起；五个信号必须全部静默 |

凭据在工具参数中以字面量 `${credential}` 书写；明文只在边界处替换，绝不进入日志、事件或审计记录。

### `AGENT_EGRESS_ENFORCEMENT=network`

`proxy`（默认）只是设置 `HTTP_PROXY`，无视它的二进制仍然有默认路由；`network` 则直接拿掉这条路由：

```bash
just egress-proxy-image                     # 构建 sidecar 镜像
export AGENT_EGRESS_ENFORCEMENT=network
export EGRESS_PROXY_IMAGE=aria-egress-proxy:local
export CONTROL_PLANE_URL=http://cloud:3000  # 代理如何访问本服务
docker compose up
```

启动时服务会创建 `aria-egress` 网络（`internal: true`），把代理容器同时接入该网络与 compose
网络，之后每个 sandbox 都以 `network_mode=aria-egress` 运行。代理不是一个 compose 服务。

| 变量 | 含义 |
|---|---|
| `EGRESS_PROXY_IMAGE` | sidecar 镜像；`network` 模式必填 |
| `AGENT_EGRESS_NETWORK` | internal 网络名（默认 `aria-egress`） |
| `AGENT_EGRESS_PROXY_URL` | sandbox 内可达的地址（默认 `http://aria-egress-proxy:8080`） |
| `CONTROL_PLANE_URL` | 代理如何访问本服务 |
| `EGRESS_PROXY_TOKEN` | 共享密钥；未设置时每次启动随机生成 |

任何一步失败（没有网络、没有镜像、代理未运行、不在容器内）服务都会**拒绝启动**，并在消息中
给出下一步动作；不会退回 `proxy` 模式。

身份就是 sandbox 的来源 IP：容器创建时打上 `aria.tenant_id` / `aria.session_id` 标签，代理据此
询问 `/internal/egress/whoami`。没有标签的来源一律拒绝；请求里声明的 principal 与来源 IP 不
匹配同样会被拒绝——被攻陷的代理读不到别的租户的策略。结果缓存 10 秒（上限 4096）：Docker 会
复用 IP，TTL 太长迟早把流量记到错误的租户头上。

覆盖范围与边界：

* 每个 sandbox 后端都会被包裹，包括浏览器后端内部的 Docker sandbox，因此 `shell`、`job_*`、
  agent loop 与 `browser_*` 都落在 internal 网络上。包裹也发生在运行时构造点，所以设置了
  `egress_network` 的嵌入式 `Agent` 同样绕不过去。`cube` 没有自定义网络与标签的能力，会显式
  拒绝而不是留下默认路由；它的代理环境通过 `-e` 传入。
* `network` 强制下 `term_*` **不注册**：它在控制面进程里驱动 PTY，任何容器网络都覆盖不到。
  需要交互式宿主终端请使用 `proxy` 模式。
* 恢复检查点同样接入 internal 网络并带上通过认证的 principal 的租户标签；缺少
  `AGENT_EGRESS_NETWORK` 时恢复会失败，而不是悄悄在默认网桥上起容器。恢复出的 sandbox 在
  `proxy` 模式下也会拿到代理环境，因此不会走直连。

`GET /v1/governance/status` 会报告 `enforcement: "network"` 以及代理的容器、网络、镜像与启动时的健康状态。

云端端点（全部按 `principal_id` 分片）：`GET|PUT|PATCH /v1/governance/egress-policy`（修订号
守卫；过期写入返回 `409`）、`GET /v1/governance/audit`、`GET /v1/governance/audit/export`、
`GET /v1/governance/usage`、`GET /v1/governance/status`、`/v1/governance/credentials` 下的凭据
元数据，以及 `/v1/governance/checkpoints` 下的检查点（恢复会派生新 sandbox，并走与创建相同的
配额闸门）。

迁移说明：四张治理表（`egress_policies`、`audit_events`、`usage_events`、
`sandbox_checkpoints`）在 `ensure_schema` 中以 `CREATE TABLE IF NOT EXISTS` 建立，已有
数据库无需手工操作。`allowlist` 模式却没给主机清单可以正常启动，但会拒绝所有请求——想明确
表达「全拒绝」请用 `deny_all`。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 📚 文档

详细设计记录位于 [`docs/adr/`](./docs/adr)，以英文撰写：

| 文档 | 覆盖内容 |
|---|---|
| [FFI 边界](./docs/adr/0002-ffi-boundary.md) | `ariacompute-agent` 稳定导出面与重新生成规则 |
| [Sandbox 提供者](./docs/adr/0003-sandbox-providers.md) | Docker / Kata / Cube / Browser provider trait |
| [Memo 存储](./docs/adr/0004-memo-storage.md) | `aria-agent-memo` 端侧存储表结构 |
| [多租户 API Key](./docs/adr/0007-multi-tenant-api-keys.md) | 管理员 / 租户密钥模型与 `POST /v1/api-keys` |
| [SDK 接口](./docs/adr/0008-sdk-interface.md) | 无会话的 happy-path SDK 人体工学 |
| [OpenAI beta Agents API](./docs/adr/0009-openai-beta-agents-api.md) | 流式事件信封契约 |
| [上下文存储 pgvector](./docs/adr/0010-context-storage-pgvector.md) | 云端上下文为何强制 pgvector |
| [记忆后端切换](./docs/adr/0011-memory-backend-switch.md) | `cloud` / `local` / `both` 选择与合并 |
| [浏览器 sandbox](./docs/adr/0012-browser-sandbox.md) | 六镜像 `BrowserCatalog` 与 `BrowserSandbox` |
| [Agent–memo 集成](./docs/adr/0013-agent-memo-integration.md) | 记忆图、自动压缩、memo 工具族 |
| [DeepSeek Harness 运行时](./docs/adr/0019-dsh-runtime.md) | `dsh` 作为默认 `agent_loop` 提供方 |
| [受控出站](./docs/adr/0020-governed-egress-single-boundary.md) | 单一判定/替换/审计边界，默认拒绝 |
| [SDK 云端客户端](./docs/adr/0020-sdk-cloud-client.md) | 原生 SDK 为何不内嵌运行时 |
| [审计结构与保留](./docs/adr/0021-audit-event-schema-and-retention.md) | 审计事件字段、JSONL sink、保留期 |
| [检查点与计量](./docs/adr/0022-sandbox-checkpoint-and-metering.md) | sandbox 快照与按状态计量 |
| [空闲与轮换](./docs/adr/0023-idle-detection-and-credential-rotation.md) | 五信号空闲检测、凭据轮换 |
| [网络强制](./docs/adr/0024-network-enforcement-sidecar.md) | internal 网络 + 无密钥出站代理 |

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 📝 许可证

MIT，详见 [LICENSE](./LICENSE)。

<div align="right">

[![][back-to-top]](#readme-top)

</div>

## 🤝 工程约定

本仓库遵循 Harness Engineering：仓库本身即 Agent 的上下文，治理它的文档与代码一同版本化。

- [`AGENTS.md`](./AGENTS.md)：Agent 工程上下文入口与目录索引
- [`requirements.md`](./requirements.md)：需求规格（功能边界 / 异常 / 验收，人审后编码）
- [`task.md`](./task.md)：实施清单

<div align="right">

[![][back-to-top]](#readme-top)

</div>

---

<div align="center">

Copyright © 2026 [ariacompute][org]. <br/>
This project is [MIT](./LICENSE) licensed.

</div>

<!-- 链接定义 -->

[org]: https://github.com/ariacompute
[release-link]: https://github.com/ariacompute/agent/releases
[language-link]: ./Cargo.toml
[license-link]: ./LICENSE
[lastcommit-link]: https://github.com/ariacompute/agent/commits
[sdks-link]: #-sdk-示例
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
