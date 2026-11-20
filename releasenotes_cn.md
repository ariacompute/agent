# 发布说明

## 统一模型网关配置

`GATEWAY_BASE_URL` / `GATEWAY_API_KEY` / `GATEWAY_MODEL` 现在同时为
`AGENT_HARNESS=dsh` 与 `native` 提供模型配置。独立的
`DEEPSEEK_BASE_URL` / `DEEPSEEK_API_KEY` 已移除，`GATEWAY_*` 也不再回退到
`OPENAI_*`。dsh 会通过其子进程白名单接收这些网关值；设置 `GATEWAY_MODEL` 时，
模型 id 由它决定。

## 受控执行：单一出站边界、审计、计量与凭据

每个出站请求现在都经过同一条边界：判定、替换、记录（ADR-0020）。开启
`AGENT_EGRESS_ENFORCEMENT=network` 后，这条边界成为**唯一**出口：sandbox 运行在
`internal` 的 Docker 网络上，唯一可达的对端是一个不持有任何密钥的代理（ADR-0024）。

### 主要变更

- **四个新接缝** —— `egress`、`audit`、`metering`、`credentials`。每个的默认实现都
  是拒绝而非放行（`DenyAllEgress`、`NoAuditConfigured`、`NoMeteringConfigured`、
  `NoCredentials`），因此漏装配就是 fail-closed。
- **新增 `aria-agent-governance`** —— `EgressGateway`（判定 → 替换
  `${credential}` → 审计 → 计量）、`LocalForwardProxy`（HTTP/1.1 absolute-URI
  转发 + `CONNECT` 盲转发）、`JsonlAuditSink`（按 UTC 日分文件、保留期清理、NDJSON
  导出）、`MeteringLedger` + `QuotaGuardSandbox`（预留被拒即回滚容器）、
  `GovernanceProjector`（只为模型投影工具输出，事件里保留原始结果）、
  `FileCredentialStore`（落盘 ChaCha20-Poly1305、轮换期限、header stub），以及基于
  五个独立信号的空闲检测。
- **新增 `aria-agent-egress-proxy`** —— 独立镜像，不持有密钥、不落盘。它通过
  `/internal/egress/*` 实现同一组接缝，所以判定、脱敏与审计语义是复用而不是另写一套。
  身份就是来源 IP：控制面按容器标签（`aria.tenant_id` / `aria.session_id`）解析，缓存
  10 秒 / 4096 个；未知来源一律拒绝，声称的 `principal_id` 与来源 IP 不符同样拒绝 ——
  被攻陷的代理也无法跨租户。
- **启动即失败，绝不降级** —— `network` 模式下，服务先创建网络、启动代理、把代理接入
  控制面网络并探测健康，任何一步失败都带着下一步动作退出。不会静默退回 `proxy` 模式。
- **结构性覆盖** —— 包裹发生在运行时装配点（`Agent::with_*`），也覆盖浏览器后端内部的
  Docker sandbox，因此 `shell`、`job_*`、agent loop 与 `browser_*` 都落在 internal
  网络上。`term_*` 不注册（宿主 PTY，容器网络覆盖不到）；`cube` 拒绝自定义网络与标签，
  但会转发代理环境变量；恢复的检查点同样接入该网络并带上通过认证的租户标签。
- **云端接口** —— `/v1/governance/*`：策略修订（过期写入返回 `409`）、审计与 NDJSON
  导出、用量、状态（强制模式 + 代理健康）、凭据元数据（配置了凭据库时），以及检查点与
  恢复。四张按 `principal_id` 分片的表（`egress_policies`、`audit_events`、
  `usage_events`、`sandbox_checkpoints`）以 `CREATE TABLE IF NOT EXISTS` 建立，已有
  数据库无需手工迁移；凭据本身仍在加密的端侧存储中。
- **可观测** —— 代理不是 compose 服务：`docker network ls` 能看到 internal 网络、
  `docker ps` 能看到 sidecar；拒绝会以
  `egress is deny-by-default and x is not on the allowlist (policy rN)` 的形式回到
  模型，而不是超时。

### 配置

| 环境变量 | 默认值 | 含义 |
| --- | --- | --- |
| `AGENT_EGRESS_MODE` | `deny_all` | `deny_all` \| `allowlist` \| `denylist` |
| `AGENT_EGRESS_ALLOW` / `AGENT_EGRESS_DENY` | *（空）* | 主机模式；`*.` 匹配子域 |
| `AGENT_EGRESS_ENFORCEMENT` | `proxy` | `proxy` \| `network` \| `off`（需 `AGENT_ALLOW_UNENFORCED_EGRESS=1`） |
| `AGENT_EGRESS_NETWORK` / `AGENT_EGRESS_PROXY_URL` | `aria-egress` / `http://aria-egress-proxy:8080` | internal 网络名与 sandbox 内可达的地址 |
| `EGRESS_PROXY_IMAGE` / `EGRESS_PROXY_TOKEN` | — / 每次启动生成 | sidecar 镜像（`network` 必填）与共享令牌 |
| `CONTROL_PLANE_URL` | *（`network` 必填）* | 代理如何访问本服务 |
| `AGENT_AUDIT_DIR` / `AGENT_AUDIT_RETENTION_DAYS` | `<workspace>/.aria/audit` / `30` | JSONL sink；`0` 表示永久保留 |
| `AGENT_METERING` / `AGENT_COMPRESSION` | `on` / `on` | 用量账本 / 面向模型的投影 |
| `AGENT_CREDENTIALS_DIR` / `AGENT_CREDENTIAL_MASTER_KEY` | *（未设置即关闭）* / 开启时必填 | 加密凭据库 |
| `AGENT_IDLE_TIMEOUT_SECS` | `900` | `0` 关闭挂起 |

### 破坏性变更

- `ExecSpec` 新增 `network` 与 `labels`，仓库外的结构体字面量不再编译；请改用
  `ExecSpec::command(..)` 并设置新字段。
- `allowlist` 模式却没给主机清单不会导致启动失败，而是在每次调用时拒绝所有请求；想明确
  表达「全拒绝」请用 `deny_all`。
- `network` 强制缺少 `EGRESS_PROXY_IMAGE`、`CONTROL_PLANE_URL` 或代理未运行时拒绝启动。

### 验证

`cargo fmt --all -- --check`、`cargo clippy --locked --workspace --all-targets
-- -D warnings`、`cargo test --locked --workspace`（941 通过）与
`./scripts/coverage.sh`（行覆盖 99.95%，25127/25140；剩余行要么是带说明的
`// LCOV_EXCL_LINE` 例外，要么是闭合定界符假象）全部通过。

## Rust / Swift / Kotlin SDK 改为云端 API 客户端

UniFFI 表面不再内嵌运行时：`ariacompute-agent` 现在是 beta Agents API 的薄 HTTP
客户端，词汇与 JS/Python SDK 一致。设备二进制里没有循环、sandbox、工具族，也不需要
模型密钥（ADR-0020）。

### 主要变更

- **新接口**：`AriaAgent`（`createSession`、`session`、`run`、`runStreamed`）、
  `Session`（`memorize` / `recall` / `itemsJson` / `turnsJson` / `titleJson`）、
  `AriaAgentConfig`、`ClientOptions`、`MemoryConfig`、`RunResult`、`TokenUsage`、
  `StreamEvent`、`StreamListener`、`AriaError`。
- **移除**：进程内运行时、`create_agent_for_tenant[_with]`、
  `sandbox_provider` / `permission_preset` / `approval_mode`，以及内嵌上下文存储。
- **记忆**仍支持 `cloud`（会话记忆 REST）、`local`（aria memo CLI）与 `both`；
  缺少 CLI 时是可直接处理的配置错误。
- **错误**类型化（`config` / `auth` / `network` / `api` / `unknown`），`api` 附带
  HTTP 状态码。
- **流式**把每个 `agent.*` 帧交给 `StreamListener`，并遵守 `maxTurns`；空流是错误，
  绝不当成空答案。
- **跨 SDK 一致**：JS 与 Python 现在遵循同一契约 —— 空流是 `unknown` 错误、未知记忆
  后端是 `config`、`local` 记忆行统一为 `key: value`（SDK 与 CLI 写入的 aria memo
  数据可互相读取），`maxTurns` 文案也完全一致。
- 绑定已用 `just ffi` 重新生成，Swift/Kotlin 调用方需按 README 示例迁移。

## 默认运行时切换为 DeepSeek Harness

每一轮对话现在默认由 **DeepSeek Harness（dsh）** 执行：dsh 以外部子进程
（`--profile sdk`）形式启动，通过 stdio 上的逐行 JSON-RPC 通信。turn 循环、模型
调用与工具执行归 dsh；本仓库继续负责 Cloud API、记忆、多租户与冻结的流式信封。

### 主要变更

- **`agent_loop` 成为真正的接缝**：`AgentLoop` + `LoopContext`；原循环原样搬进
  `NativeLoop`，可用 `AGENT_HARNESS=native` 显式回退（ADR-0019）。
- **新增 `aria-agent-dsh`**：stdio 传输（8 MiB 帧上限、stderr 排空）、带超时且
  失败即拒绝的 JSON-RPC 客户端、按会话复用的运行时池（握手一次、空闲回收、容量
  上限），以及唯一的纯函数映射模块（dsh 通知 → `AgentEvent` / `SessionEvent`）。
- **审批与提问桥接**到已安装的接缝：缺少提供方即拒绝并回传原因，绝不默认放行。
  每个会话独占 `DSH_HOME` 与工作目录；子进程环境为白名单。
- **接缝契约对齐 dsh**：四类角色（`core`/`seam`/`bundle`/`service`），dsh 拼写作为
  别名接受（`approval`、`terminals`、`spillStore` 等），注册顺序即优先级，
  `dispose` 逆装配顺序释放；`service` 键拒绝注册 provider。
- **新增任务**：`just dsh-smoke`（假运行时，离线）与 `just dsh-e2e`（真实一轮；
  缺少 `ARIA_DSH_E2E=1`、`dsh` 或凭据时明确跳过）。

### 配置

| 环境变量 | 默认值 | 含义 |
| --- | --- | --- |
| `AGENT_HARNESS` | `dsh`（云二进制） | `dsh` 或 `native` |
| `DSH_BIN` | `PATH` 上的 `dsh` | 运行时可执行文件 |
| `DSH_HOME_ROOT` | `.aria/dsh` | 每租户/每会话的 home 根目录 |
| `DSH_POOL_MAX` | `8` | 每租户活跃运行时上限 |
| `DSH_IDLE_TTL_SEC` | `900` | 空闲回收窗口 |
| `DEEPSEEK_BASE_URL` / `DEEPSEEK_API_KEY` | — | OpenAI 兼容端点 |
| `ARIA_AGENT_HARNESS` | `native` | 原生 SDK 显式启用 |

## 早期：能力接缝内核，移除内置 harness

本次发布用可组合的 agent 内核取代原先基于 codex 的单循环 harness。每一项能力
都是一个带有默认提供方的能力接缝；Cloud API（OpenAI beta Agents 兼容）、UniFFI
表面与 JS/Python SDK 依然是唯一对外出口。

### 主要变更

- **插件内核**（`aria-agent-plugin`）：`PluginManifest`、`SeamKey`、`SeamRole`
  与 `Registry` 构建器。装配时按 `requires` 拓扑排序，拒绝重名 id、`core` 键多
  提供方、缺失依赖与成环，成功后冻结。`Agent::with_handlers` 一次性解析全部
  接缝，热路径不再查表（ADR-0014）。
- **统一工具管线**：解析 → 权限预设 → 审批 → 分发 → 结果后处理。无审批方时调用
  被判为 `unavailable` 并拒绝，原因回灌模型；超长结果截断并外置存储。`fs` /
  `shell` / `jobs` 共享单一 sandbox 根目录；`web_fetch` 施加 SSRF 防护、重定向与
  响应体上限以及 content-type 白名单（ADR-0016）。
- **会话事件流成为唯一事实来源**：`SessionEvent`（`turn_started`、`step`、
  `item_*`、`text_*`、`approval`、`usage`、`compacted`、`title_set`、`turn_*`）、
  批量 flush 且失败可重试的 `SessionRecorder`，以及 `SessionProjection::replay`。
  本地为 JSONL，云端表是投影。用量优先取模型上报，缺失时本地估算；上下文超限
  压缩一次并重试一次（ADR-0015）。
- **标题全链路可用**：确定性的首条输入标题（只写一次）加可选异步提供方覆盖。
  `run`、`run_stream`、`run_event_stream` 三个入口记录同一套回合事件，并在调用
  模型前 flush 标题，因此被委派的子 agent 也会得到可寻址的会话标题。
  `GET /v1/agents/sessions/{id}/title` 读取投影；子会话 id
  （`<parent>::sub::<id>`）通过父会话的租户归属鉴权。
- **多智能体与调度**（`aria-agent-orchestrator`）：进程内子智能体（独立命名空间、
  并发上限、可缓存的 `wait`）；经校验的任务 DAG 团队（依赖门控认领）；定时任务
  先推进 `next_run_ms` 再执行，并通过单飞 ticker 防重复（ADR-0017）。
- **真实 socket 的 E2E 套件**：生产 axum router 绑定到临时回环端口，由真实 HTTP
  客户端驱动，覆盖 beta Agents 生命周期、SSE 帧、工具调用、租户隔离、记忆与异常
  路径。Postgres/pgvector 与 Docker 场景在缺少依赖时打印跳过原因。

### 新增 crate

- `aria-agent-plugin` — manifest、注册表、装配校验、冻结句柄。
- `aria-agent-tools` — fs、shell、jobs、terminal（`pty`，默认关闭）、skills、
  web（`web`，默认关闭）、ask_user、plan、todo、memo 工具族；memo 工具从
  `aria-agent-core` 迁出。
- `aria-agent-orchestrator` — `subagent_*`、`team_*`、`schedule_*` 工具。

`aria-agent-core` 拆分为 `agent_loop`、`prompt`、`token_meter`、`seams/*` 与
`session/*`；它依旧不依赖任何具体存储实现。

### 云端 API 与 SDK

- 新增迁移 `0006_scheduled_jobs`、`0007_agent_teams`、`0008_session_titles`
  （幂等，均带 `principal_id`）。
- 新增路由 `/v1/agent-teams`、`/v1/scheduled-jobs` 与
  `GET /v1/agents/sessions/{id}/title`；编排工具族按 principal 注入，子智能体
  使用深度为 1 的子 agent 工厂。
- SSE 新增 `agent.turn.approval.requested`、`agent.turn.approval.resolved` 与
  `agent.turn.usage` 帧；原有帧序列与终态语义不变。
- `SdkAgentConfig` 新增 `permission_preset` 与 `approval_mode`；Swift/Kotlin
  绑定已通过 `just ffi` 重生成。JS 与 Python 解码新帧并暴露 `result.usage`
  （`TokenUsage`）。

### 破坏性变更

- 删除 `codex` 子模块、全部 codex git 依赖、`rama-*` 精确版本钉、
  `[patch.crates-io]` 段、`CodexSandbox` 与 `SandboxProvider::Codex`（ADR-0018）。
- `sandbox_provider = "codex"` 现在是硬配置错误，不再静默回退；默认沙箱为
  docker，kata/cube/browser 仍可选择。
- 没有审批提供方的部署将拒绝写操作，而非隐式放行；无人值守写入需要显式配置
  `approval_mode = auto` 与写权限预设。
- 已废弃的 `Agent::new` / `with_model` / `with_sandbox` 仍作为薄包装保留；
  `shell` 工具名与参数形状保持不变。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace`（641 通过）、`just e2e`（5 通过；DB/Docker 场景缺依赖时跳过）、
`just ffi`、`just sdk-js-test`（53 通过）与 `just sdk-py-test`（30 通过）均通过。
