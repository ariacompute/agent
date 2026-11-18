# 发布说明

## 能力接缝内核，移除内置 harness

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
