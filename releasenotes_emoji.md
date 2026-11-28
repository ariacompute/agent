# 发布说明 ✨

## 🐳 dsh 跑进容器了：默认开启，失败即失败

原来的默认运行时是**宿主子进程**：`dsh` 既调模型也跑它自己的工具，唯一的边界只是权限档。现在整条会话在容器里，宿主进程不再充任边界。

### 🧱 沙箱接缝补齐

- 🔌 `Sandbox` 新增 `exec_attached`（一次长连接 `exec`，双向 stdio）、`copy_in`（tar 上传）、`has_image`；`ExecSpec` 新增 `mounts` / `hardening` / `resources`。默认实现是 `Unsupported`，旧后端不改也能编译。
- ⏱️ 容器跑 keep-alive 而不是 `sleep 3600`：workspace 在容器内准备，再 attach `dsh --profile sdk`，容器活多久会话就活多久。
- 📦 会话工作区落在**命名卷** `aria-dsh-<tenant>-<session>`，不是 bind —— 云端经 `/var/run/docker.sock` 与守护进程对话，bind 源会解析到守护进程那台机器上。
- 🧯 `SandboxShell` 自己强制 `timeout_ms`，超时报 `Unavailable`，并且每条路径都销毁容器。

### 🛡️ 默认与失败语义

- ✅ `DSH_SANDBOX` 默认 `1`：镜像缺失（从不拉取）、workspace 准备失败、技能册上传被拒、harness 无法 attach，任一项都中止启动或销毁容器。
- 🚪 `DSH_SANDBOX=0` 是显式且会打日志的逃生门，绝不静默降级。
- 🧠 thinking 过滤器区分「监听地址」与「容器可达地址」：`DSH_THINKING_FILTER_BIND` / `_URL` 缺一个就在启动时失败，而不是第一轮才炸。
- 💰 harness 容器有独立预算（`DSH_SANDBOX_CPUS/MEMORY/PIDS`，默认 1 CPU / 1 GiB / 512 pids），加入受治理的出网网络并带租户/会话标签。
- 🏗️ `docker build --target sandbox` 产出镜像（uid 10001、`WORKDIR /aria` 不落在挂载点里、pip 走镜像源并可续传），阶段置于最前，`--target sandbox` 不再连带编译整个 workspace。

### ⚠️ 遗留风险（ADR-0027）

- `EGRESS_MODE=network` 下 dsh 是否遵守 `HTTPS_PROXY` 未验证；过滤器需要 sidecar。
- `cap_drop=ALL` 可能与 dsh 自带 sandbox 冲突，回退 `DSH_SANDBOX_HARDEN=0`。
- 1 GiB 是初值；凭据经环境变量进入容器，`docker inspect` 可见；harness 容器不计入 `QuotaGuardSandbox`。

### ✅ 验证

`cargo test --workspace`（1287 ✅）、`fmt`、`clippy -D warnings` 与覆盖率闸门（计入统计的行 35073/35073，100%；测量值 99.98%，仅剩 7 个闭合括号伪影）全部通过；`just sandbox-image` 构建出 650 MB 镜像，Docker 原语的 gated 用例（`ARIA_E2E_DOCKER=1`，2 条）在真实守护进程上通过，`ARIA_DSH_SANDBOX_E2E=1` 跑通容器内 `initialize` 握手。

## ♻️ 递归自我改进：账本变成能力 🧠

每次任务留下的 `SessionEvent` 账本，现在还会被折成诊断：病症 → 候选 → 重放打分 → 人工闸门 → 新版本。没有第二套事件流、没有后台守护、也不热替换。账本与诊断**默认开启**，唯一花钱的真实重跑默认关闭。

### 🧠 诊断

- 📒 `session::insight` 纯折叠出 `EfficiencyReport` / `TurnInsight` / `Pathology`，每条结论都带 `seq` / 工具 / 轮次证据。
- 🔍 六类具名病症：同一工具反复失败、重试风暴、审批被拒循环、压缩抖动、步数预算耗尽、工具参数反复修补。
- 🧱 `ChainPersistence`：审计 sink 权威、账本尽力而为，账本故障不会把已完成的回合变成失败。
- ➕ `ItemDone` 只加可选 `latency_ms`，不加事件变体 —— 旧 JSONL 与冻结的 SSE 信封都不受影响。

### 🧪 验证与闸门

- 🧩 新增 `aria-agent-rsi`：账本去重、规则表提案（零模型调用）、重放打分、带版本号的手册引擎、技能落盘与单飞 sweep。
- 📉 重放增益低于 `min_score_gain` 直接丢弃；没人批准就不发布，`expected_revision` 过期返回 `409`。
- 🌍 跨租户复制缺少适配记录返回 `422` 且不产出资产；非管理员适配返回 `403`。
- 🧾 可选真实重跑：`ModelRerunProbe` 真的调一轮模型（无工具、无沙箱副作用），每次尝试都把 token / 步数 / 耗时 / 结论写进 `rsi_reruns`；超时、报错、空回答、回执写不进去都不算通过。

### ☁️ 云端与复用

- 🛰️ `/v1/rsi/*`：status、sweep、reports、proposals（promote / discard）、playbooks（versions / adapt）；关闭时只有 `POST /v1/rsi/sweep` 返回 `503`，只读查询仍可用。
- 🗄️ `0009_rsi.sql` 建四张按租户分片的表（含重跑回执），晋升在单事务内完成，唯一索引兜住并发发布。
- 🧰 晋升技能落到共享目录 `~/.ariacompute/skills`（`FilesystemSkills` 与 dsh 读的是同一份），手册只是下一轮读取的新版本，绝不热替换运行中的提示词。

### ✅ 验证

`cargo test --workspace`（1150 ✅）、`fmt`、`clippy -D warnings` 与覆盖率闸门（计入统计的行 31650/31650，100%）全部通过。

## 🛡️ 受控执行：出站、审计、计量与凭据 🔐

每个出站请求都经过同一条边界：判定 → 替换 `${credential}` → 审计。开启 `AGENT_EGRESS_ENFORCEMENT=network` 后，这条边界成为唯一出口：sandbox 在 `internal` 网络上运行，唯一可达的对端是不持任何密钥的代理。

### 🚦 出站

- 🧭 `egress` / `audit` / `metering` / `credentials` 四个新接缝，默认实现一律拒绝（漏装配即 fail-closed）。
- 🧾 拒绝会带着策略修订号回到模型：`egress is deny-by-default and x is not on the allowlist (policy rN)` —— 不是超时。
- 🏷️ 策略带修订号，云端过期写入返回 `409`。

### 🐳 网络强制

- 🕳️ sandbox 没有默认路由：无视 `HTTP_PROXY` 的结果是失败，不是泄露。
- 🧊 代理镜像不含密钥、不落盘；身份 = 容器标签 + 来源 IP，缓存 10 秒 / 4096 个。
- 🛑 网络、代理、控制面网络、健康探测任一失败即拒绝启动，绝不降级为 `proxy`。
- 🧱 运行时装配点与浏览器后端内部 sandbox 都被包裹，`shell` / `job_*` / `browser_*` 落在同一网络；`term_*` 在强制模式下不注册。

### 🔐 审计、计量与凭据

- 📜 JSONL 审计按 UTC 日分文件、可设保留期、支持 NDJSON 导出。
- 📊 用量按生命周期状态计量；预留被拒会回滚容器。
- 🗝️ 凭据以 ChaCha20-Poly1305 落盘，支持轮换期限与 header stub；明文只在边界替换，不进日志、事件或审计。
- 📉 工具输出只为模型投影，事件保留原始结果。
- ⏱️ 五个信号全部静默才判定空闲（`AGENT_IDLE_TIMEOUT_SECS`，`0` 关闭）。

### ⚠️ 破坏性变更

- 🧷 `ExecSpec` 新增 `network` / `labels`，仓库外的结构体字面量需改用 `ExecSpec::command(..)`。

### ✅ 验证

`cargo test --workspace`（941 ✅）、`fmt`、`clippy -D warnings` 与覆盖率闸门（99.95% 行覆盖）全部通过。

## ☁️ Rust / Swift / Kotlin SDK 改为云端 API 客户端 🚀

UniFFI 表面不再内嵌运行时：`ariacompute-agent` 现在是 beta Agents API 的薄 HTTP 客户端，词汇与 JS/Python SDK 一致。设备二进制里没有循环、sandbox、工具族，也不需要模型密钥。

### 🧩 新接口

- 🤖 `AriaAgent`（`createSession`、`session`、`run`、`runStreamed`）与 `Session`（`memorize` / `recall` / `itemsJson` / `turnsJson` / `titleJson`）。
- 🧾 配套类型：`AriaAgentConfig`、`ClientOptions`、`MemoryConfig`、`RunResult`、`TokenUsage`、`StreamEvent`、`StreamListener`、`AriaError`。

### ⚠️ 破坏性变更

- 🗑️ 移除进程内运行时、`create_agent_for_tenant[_with]`、`sandbox_provider` / `permission_preset` / `approval_mode`，以及内嵌上下文存储。
- 🔁 绑定已用 `just ffi` 重新生成，Swift/Kotlin 调用方需按 README 示例迁移。

### ✅ 行为保证

- 🧠 记忆仍支持 `cloud`（会话记忆 REST）、`local`（aria memo CLI）与 `both`；缺少 CLI 时是可直接处理的配置错误。
- 🚦 错误类型化（`config` / `auth` / `network` / `api` / `unknown`），`api` 附带 HTTP 状态码。
- 📡 流式把每个 `agent.*` 帧交给 `StreamListener`，并遵守 `maxTurns`；空流是错误，绝不当成空答案。
- 🤝 跨 SDK 一致：JS / Python 与 Rust / Swift / Kotlin 同契约 —— 空流是 `unknown` 错误、未知记忆后端是 `config`、`local` 记忆行统一为 `key: value`（aria memo 数据可互相读取）、`maxTurns` 文案一致。

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
