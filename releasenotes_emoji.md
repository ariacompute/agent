# 发布说明 ✨

## 🐋 默认运行时切换为 DeepSeek Harness

- 🧵 `agent_loop` 成为真正的接缝（`AgentLoop` + `LoopContext`），原循环原样保留为 `NativeLoop`，`AGENT_HARNESS=native` 可显式回退。
- 🔌 新增 `aria-agent-dsh`：stdio JSON-RPC 传输、按会话复用的运行时池、审批/提问桥接、唯一的通知→事件映射模块。
- 🛡️ 每个会话独占 `DSH_HOME` 与工作目录，子进程环境为白名单；无审批方即拒绝。
- 🧪 `just dsh-smoke`（假运行时，离线）与 `just dsh-e2e`（真实一轮，缺依赖时明确跳过）。

## 🧩 能力接缝上位，内置 harness 退场 🚀

发布说明的精简速览版。

### 🧩 新内核

- 🔌 每项能力（模型、sandbox、存储、会话、工具、审批、子智能体、调度）现在都是可替换提供方的能力接缝。
- 🧱 `aria-agent-plugin` 在启动时校验插件图：无重复 id、无冲突 `core` 提供方、无环、无缺失依赖，随后冻结 —— `Agent` 一次性解析全部接缝，热路径不再查表。
- 🛠️ 统一工具管线：解析 → 预设 → 审批 → 分发 → 截断/外置。🚫 没有审批提供方时直接拒绝，绝不静默放行。

### 💬 会话与标题

- 📜 `SessionEvent` 是唯一事实来源；本地以 JSONL 批量 flush，云端为投影表。📊 优先采用模型上报的 token 用量，本地估算仅作兜底；上下文超限压缩一次并重试一次。
- 🏷️ 确定性的首条输入标题 + 可选异步提供方覆盖。三个循环（`run`、`run_stream`、`run_event_stream`）均在调用模型前记录并 flush 标题。
- 👶 子会话（`<parent>::sub::<id>`）同样带有标题，并可通过 `GET /v1/agents/sessions/{id}/title` 寻址，按父会话租户鉴权。

### 🤖 编排

- 🧒 进程内子智能体：独立命名空间会话、并发上限与可缓存 `wait`（云端委派深度为 1）。
- 🗂️ 智能体团队是经校验的任务 DAG：环、重复 id 与悬空 id 均被拒绝；认领受依赖关系门控。
- ⏰ 定时任务先推进 `next_run_ms` 再执行，因此重启或第二个副本都不会在同一时刻重复触发。

### ☁️ 平台与 SDK

- 🗄️ 新增幂等且按 `principal_id` 隔离的表：`scheduled_jobs`、`agent_teams`、`session_titles`；新增 `/v1/agent-teams`、`/v1/scheduled-jobs` 与 `GET …/title` 路由。
- 📡 新增 SSE 帧：`agent.turn.approval.*` 与 `agent.turn.usage` —— 原有帧与终态语义不变。
- 📱 `SdkAgentConfig` 新增 `permission_preset` / `approval_mode`；Swift/Kotlin 绑定已重生成，JS/Python 暴露 `result.usage`。

### ⚠️ 破坏性变更

- 🗑️ 删除 `codex` 子模块、其 git 依赖、`rama-*` 精确版本钉、patch 段、`CodexSandbox` 与 `SandboxProvider::Codex`。
- 🛑 `sandbox_provider = "codex"` 现在启动即失败；默认沙箱为 docker。
- 🔒 没有审批代理的部署将拒绝写操作，而非隐式放行。

### 🧪 E2E

- 🌐 真实回环 socket 驱动 beta Agents API：生命周期、SSE、工具、租户、记忆与异常路径。
- 🗄️ Postgres（`DATABASE_URL`）与 🐳 Docker（`ARIA_E2E_DOCKER=1`）场景在缺少依赖时打印原因并跳过。

### ✅ 验证

`cargo test --workspace`（641 ✅，含 5 个 E2E）、JS SDK（53 ✅）、Python SDK（30 ✅），以及 `fmt`、`clippy -D warnings` 与 `just ffi` 全部通过。
