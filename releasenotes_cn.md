# 发布说明

## 同一个端口现在也是 A2A 远程 agent

`aria-agent-cloud` 原本只说一种协议：OpenAI beta Agents API。agenthub 这类编排器无法把它注册成远程
agent——它按 Agent Card 约定发现对端，再用 A2A JSON-RPC 驱动。于是同一个进程现在也说这套协议
（ADR-0028）：同一个端口、同一把 key、同一个会话存储。它是第二层投影而不是重写——`/v1/agents/**`、
冻结的 SSE 信封与各 SDK 都没改，也没有新增权限码（`write` 覆盖 `message/*`、`cancel`、`resume` 与
push `set`；`read` 覆盖 `get`、push `get` 与重放流）。

### 主要变更

- **独立的协议 crate** —— `aria-agent-a2a`（不依赖 axum / sqlx，无 I/O）承载卡片与任务类型、
  JSON-RPC 信封与错误码表、`Last-Event-ID` 解析与 RFC 3339 辅助。`protocolVersion` 为 `1.0.0`。
  卡片只带 agent 名与 `A2A_CARDS_FILE` 的 description/skills——绝不带 `instructions`、凭据、主体或
  沙箱名——也不宣传原生工具族，因为默认 dsh harness 的工具跑在容器里。
- **卡片公开，方法不公开** —— `/.well-known/agent-card.json`（别名 `agent.json`、
  `/a2a/agent-card`）与 `/v1/agents/{id}/agent-card.json` 像 `/healthz` 一样在 `require_auth`
  **之后**合并；JSON-RPC 与重放流在它**之前**注册，因此每个方法都继承 Bearer 鉴权，并在分发器里
  再做一次 scope 判定。未知 agent 返回 `404 {"error":{"code":"agent_not_found"}}`；
  `A2A_DEFAULT_AGENT` 留空时 well-known 卡片返回同一机器码，而按 agent 的路由照常可用。
- **方法级错误装在 HTTP 200 里** —— 非 2xx 会被调用方读成 `resource_unreachable`，所以
  `-32001`…`-32009` 以 JSON-RPC error 对象返回，机器名放在 `error.data.code`
  （`task_not_found`、`invalid_state_transition`…）。只有信封损坏是 `400`；缺令牌或令牌错误是
  `401`，缺 scope 是 `403 insufficient_scope`。
- **`contextId` 就是会话，一个 context 只跑一个任务** —— 它映射到 `agent_sessions.id`，因此 A2A
  会话复用同一个 dsh 容器与卷，也会作为普通 turn 出现在控制台里。第二个 `message/send` 打到忙碌的
  context 得到 `-32005`，而不是两条交错的运行；未知 context 会新建 `sess_<uuid>`。
- **`tasks/cancel` 销毁容器** —— 先 CAS 状态、放掉挂起的决议、abort 运行，再由 `cancel_harness`
  驱逐会话运行时（`DshPool::evict`），于是沙箱传输层销毁容器，而卷保留给下一次提示。重启后
  `fail_orphans` 把遗留的行收尾。
- **流可重放** —— `GET /a2a/stream?task=&since=` 认 `Last-Event-ID`，然后跟实时任务；帧是纯 SSE
  （没有 JSON-RPC 信封），同时带 `kind` 与旧的 `event`。token 增量只广播、不落库，所以重连拿到的是
  状态、工具跨度与最终产物。
- **HITL 把任务停住** —— `PausingHitl` 同时对接审批与提问两个接缝。`tasks/resume` 只认
  `metadata.decision`；其他文本是 `-32602` 不匹配，任务**继续**停在 `input-required`，绝不隐式
  拒绝。native 注入同一个对象给 `ApprovalBroker` / `UserQuestions`；dsh 经 `SessionTag` 注入的
  `sessionId` 到达。
- **push 回调节制很小** —— `tasks/pushNotification/set|get` 或 `message/send` 的 `configuration`
  块注册一个 `http(s)` 地址，不允许 `user:pass@`；不跟随重定向，每次投递 5 秒、只重试一次，失败记进
  `push_last` 而不回滚任务状态。
- **存储** —— `migrations/0010_a2a_tasks.sql` 建 `a2a_tasks` 与 `a2a_task_events`（只存粗粒度帧，
  每条查询都按 `principal_id` 过滤）。内存 provider 覆盖同样的离线语义。

### 配置

| 变量 | 默认值 | 含义 |
| --- | --- | --- |
| `A2A_DEFAULT_AGENT` | _（空）_ | `POST /a2a` 与 well-known 卡片背后的 agent；留空时回 `agent_not_found`，`/v1/agents/{id}/a2a` 仍可用 |
| `A2A_PUBLIC_URL` | `http://127.0.0.1:<端口>` | 调用方实际访问本服务的源；卡片广播 `{A2A_PUBLIC_URL}/a2a`。卡片无法从被代理的请求推断，真实部署必须显式设置 |
| `A2A_CARDS_FILE` | _（空）_ | 可选的 camelCase JSON，覆盖卡片的 `description` 与 `skills`；文件损坏会启动失败，而不是被忽略 |

### 破坏性变更

- `AppState` 新增 `a2a` 与 `cancel_harness`；本 crate 之外的字面量构造需补
  `a2a: A2aState::disabled()` 与 `cancel_harness: None`。
- `A2aRuntime::open` 返回 `Result<TaskChannel, ContextBusy>`：调用方必须自己决定「context 忙」的
  含义，而不是让两条运行交错。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace`（1386 通过）与 `./scripts/coverage.sh`（计入统计的行 37218/37218 全覆盖）
全部通过。真实 `pgvector/pgvector:pg16` 容器下云端 318 条通过，含两条 `a2a::pg` 用例与 `0010`
建出两张表。接真 Postgres 的 curl 冒烟：无钥读到公开卡片、无 Bearer 得 `401`、未知任务 HTTP 200
+ `-32001`、重放流、在 dsh 容器内经 thinking 过滤代理真实 `message/send` 得到 `pong`、对运行中的
任务 `tasks/cancel` 变为 `canceled` 且恰好销毁一个容器；`scripts/test-api.sh` 38/38。`just dsh-e2e`
在宿主 dsh 上跑通真实网关的 3 条用例，native 模式下同一提示词在 `/a2a` 与
`/v1/agents/sessions/{id}/events` 都完成。

## dsh 现在跑在容器里

默认的 turn 运行时原本是**宿主子进程**：`dsh --profile sdk` 在与控制面同一个文件系统上执行
模型调用**和它自己的工具**，唯一的边界是 dsh 的 `workspace-write` 权限档——那是一条逻辑规则，
不是文件系统边界。ADR-0027 把这条会话搬进容器，且**默认开启**。

### 主要变更

- **会话容器，不是命令容器** —— sandbox 接缝补上 `exec_attached`（一次长连接 `exec`，双向
  stdin/stdout）、`copy_in`（tar 上传）与 `has_image`；`ExecSpec` 补上 `mounts` / `hardening` /
  `resources`。新增的 `Sandbox` 方法默认 `Unsupported`，所以既有后端不改也能编译。
- **keep-alive + attach（形态 F2）** —— 否决 `attach_container`：Docker 的
  `NewlineLogOutputDecoder` 假定 tty，而 harness 说的是 8 字节帧的 JSON-RPC。改为让容器跑
  `while true; do sleep 3600; done`，在容器内准备 workspace，然后才 attach
  `dsh --profile sdk`。原来 1 小时的上限消失：容器活多久，会话就活多久。
- **命名卷而非 bind** —— 云端经 `/var/run/docker.sock` 与守护进程对话（DooD），bind 源路径会在
  **守护进程那台机器**上解析。会话落在 `aria-dsh-<tenant>-<session>`，也正是因此，会话在空闲
  回收后、以及容器重建后都能找回状态。控制面与守护进程确实共享文件系统时可用
  `DSH_SANDBOX_MOUNT=bind`。
- **失败即失败** —— `DSH_SANDBOX` 默认 `1`；镜像缺失（任何地方都不拉取）、workspace 准备失败、
  技能册上传被拒、harness 无法 attach，都会中止启动或销毁容器。没有静默的宿主回退——那会把
  "已沙箱化"变成一句没人校验的声明。`DSH_SANDBOX=0` 是显式、会打日志的逃生门。
- **thinking 过滤器认识容器** —— 容器里的 `127.0.0.1` 就是容器自己，所以
  `DSH_THINKING_FILTER_BIND` 决定代理监听在哪，`DSH_THINKING_FILTER_URL` 是 harness 实际拨号
  的地址。沙箱模式下开着过滤器却没有可达的非回环 URL，会在启动时就失败，而不是拖到第一轮。
- **独立预算** —— harness 容器不是一条 shell 命令：它有单独的
  `DSH_SANDBOX_CPUS/MEMORY/PIDS`（默认 1 CPU / 1 GiB / 512 pids），加入受治理的出网网络并带上
  租户/会话标签，`GATEWAY_*` 与代理变量经白名单注入——绝不透传宿主环境。
- **`SandboxShell` 自己管超时** —— 超过 `timeout_ms` 的命令报 `Unavailable`，且无论走哪条路径
  （含超时路径）都会销毁容器。
- **镜像可构建** —— `docker build --target sandbox` 产出它（venv dsh 软链到
  `/usr/local/bin/dsh`、uid 10001、`WORKDIR /aria`，避免 runc 在会话挂载点里建出 root 属主的
  目录）。该阶段置于最前，否则旧版 builder 会为 `--target sandbox` 编译整个 workspace；pip
  默认走镜像源并开启续传——约 100 MB 的 runtime wheel 从 pypi.org 在消费级网络上下不完。

### 配置

| 变量 | 默认值 | 含义 |
| --- | --- | --- |
| `DSH_SANDBOX` | `1` | 容器化 harness；镜像或 workspace 不可用则启动失败 |
| `DSH_SANDBOX_IMAGE` | `aria-dsh-sandbox:latest` | 由 `just sandbox-image` 构建；从不拉取 |
| `DSH_SANDBOX_MOUNT` | `volume` | `volume`（`aria-dsh-<tenant>-<session>`）或 `bind`（目录必须对 `DSH_SANDBOX_USER` 可写） |
| `DSH_SANDBOX_ROOT` / `DSH_SANDBOX_BIN` | `/aria/session` / `/usr/local/bin/dsh` | 容器内挂载点与可执行文件 |
| `DSH_SANDBOX_VOLUME_PREFIX` | `aria-dsh` | 卷名前缀 |
| `DSH_SANDBOX_CPUS` / `DSH_SANDBOX_MEMORY` / `DSH_SANDBOX_PIDS` | `1.0` / `1g` / `512` | harness 容器预算 |
| `DSH_SANDBOX_USER` / `DSH_SANDBOX_HARDEN` | `10001:10001` / `1` | 非 root 用户；丢弃全部 capability + `no-new-privileges` |
| `DSH_THINKING_FILTER_BIND` / `DSH_THINKING_FILTER_URL` | `127.0.0.1:0` / _（空）_ | 过滤器监听地址 / 容器可达地址 |

### 破坏性变更

- `build_host_config(res, runtime, spec: &ExecSpec)` 取代了原 `network: Option<&str>` 参数；
  仓库外调用方需传 spec（挂载、加固与限额都从它读）。
- `DshConfig` 新增 `session_root` / `session` / `sandbox`；请用
  `DshConfig::for_session_like(..)` 或 `..Default` 式构造。
- `DSH_SANDBOX` 默认开启：用 `dsh` harness 的部署现在必须本地已有镜像。要保留旧的宿主子进程
  行为（以及它更弱的边界），设 `DSH_SANDBOX=0`。
- 显式留空的 `DSH_SANDBOX_IMAGE` / `DSH_SANDBOX_BIN` / `DSH_SANDBOX_VOLUME_PREFIX` 现在是错误，
  不再回落默认值。

### 遗留风险（记录在 ADR-0027，本轮未解决）

- `EGRESS_MODE=network` 会移除默认路由；dsh 在该模式下是否遵守 `HTTPS_PROXY` 未经验证，
  过滤器代理还需要 sidecar。
- `cap_drop=ALL` 可能弄坏 dsh 自带的内部 sandbox；回退是 `DSH_SANDBOX_HARDEN=0`。
- 1 GiB 是初值：真实回合被 OOM kill 就调高 `DSH_SANDBOX_MEMORY`。
- 凭据以环境变量进入容器，`docker inspect` 能看到。
- harness 容器不计入 `QuotaGuardSandbox` 的计量。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace`（1290 通过）与 `./scripts/coverage.sh`（计入统计的行
35073/35073 全覆盖；测量值 99.98%，7 个闭合括号伪影不计入）全部通过。
`just sandbox-image` 构建出 650 MB 镜像，Docker 原语的 gated 用例对
（`ARIA_E2E_DOCKER=1`，2 条）在真实守护进程上通过，`ARIA_DSH_SANDBOX_E2E=1`
完成容器内的 `initialize` 握手。

## 递归自我改进：让账本变成能力

每次任务本来就会留下 `SessionEvent` 账本。RSI（ADR-0026）把同一份账本折成诊断，
把诊断出的病症变成手册或技能候选，用候选自己的历史重放给它打分，再经带版本号的人工
闸门才发布。没有第二套事件流、没有后台守护、也不热替换。账本与诊断**默认开启**；
唯一花钱的真实重跑保持默认关闭。

### 主要变更

- **`aria-agent-core` 里的纯内核** —— `session::insight` 把记录折成
  `EfficiencyReport` / `TurnInsight` / `Pathology`，每条结论都带证据（`seq`、工具、
  轮次）。报告给的是六类具名病症（同一工具反复失败、重试风暴、审批被拒循环、上下文
  压缩抖动、步数预算耗尽、工具参数反复修补），而不只是一个分数；单份报告受
  `max_records` / `max_turns` 约束，并显式给出 `truncated` 标记。
- **事件契约只做加法** —— `SessionEvent::ItemDone` 增加
  `latency_ms: Option<u64>`（`#[serde(default)]`）；不新增变体，因此旧 JSONL 仍可解析、
  冻结的 SSE 信封不变。harness 不测耗时时，报告退回步数/重试维度而不是报错。
- **`ChainPersistence`** —— 一次写入按顺序扇出到多个 sink：第一个是权威（失败即整体失败，
  `SessionRecorder` 会重试），其后为尽力而为。云端仍是标题投影在前、RSI 账本作为可选的
  第二个 sink，因此账本出问题不会把已完成的回合变成失败回合。
- **新增 `aria-agent-rsi`**（`publish = false`，只依赖 `aria-agent-core`）——
  `RsiLedger`（按 `(principal, session, seq)` 去重、有界窗口）、
  `DeterministicReflection`（病症 → 候选的规则表，带作用域与参数清单，**零模型调用**）、
  重放 `validate`（增益低于 `min_score_gain` 即丢弃）、`PlaybookEngine`
  （`current` / `propose` / `promote` / `list_versions`）、`FilesystemSkillSink`，
  以及单飞的 `RsiSweep`（并发调用返回 `busy`）。
- **两个新接缝** —— `rsi`（`ReflectionEngine`）与 `playbook`（`PlaybookStore`），已登记
  进 `SeamKey::ALL`；未登记的键仍会在启动期 `UnknownSeam`，未接线的
  `NoReflection` / `NoPlaybooks` 一律 fail-closed。
- **云端接口** —— `GET /v1/rsi/status`、`POST /v1/rsi/sweep`、
  `GET /v1/rsi/reports` 与 `/{id}`、`GET /v1/rsi/proposals`、
  `POST /v1/rsi/proposals/{id}/promote` 与 `/discard`、
  `GET /v1/rsi/playbooks/{key}` 与 `/versions`、
  `POST /v1/rsi/playbooks/{key}/adapt`。RSI 关闭时 `POST /v1/rsi/sweep` 返回 `503`
  （只读查询仍可用）；`expected_revision` 过期返回 `409`（候选留在待审）；未适配的
  跨租户复制返回 `422` 且不产生任何资产；非管理员适配返回 `403`；未知 id 返回 `404`。
  对外视图不含 `principal_id`。
- **存储** —— `migrations/0009_rsi.sql` 新增 `rsi_reports`、`rsi_proposals`、
  `rsi_playbooks`、`rsi_reruns`（幂等、按 `principal_id` 分片）。所有查询都过滤租户，晋升在单事务内
  完成，`UNIQUE (principal_id, domain, key, revision)` 让并发发布在约束上失败，而不是
  互相覆盖。内存 provider 覆盖同一套语义，离线可测。
- **真实重跑闸门** —— `ModelRerunProbe` 把一条记录过的任务在候选手册下真实跑一轮模型。
  探针按设计不挂工具，因此验证不会变成第二次生产事故；每次尝试都往 `rsi_reruns` 写回执
  （prompt/completion token、模型轮次、工具调用 = 0、耗时、结论）。超时、模型报错、空回答、
  回执写不进去，任何一条都不算通过。因为花钱，它保持默认关闭（`AGENT_RSI_REAL_RERUN`）。
- **复用而非热替换** —— 晋升产出的是「下一轮读取」的新**版本**；技能落到共享目录
  `AGENT_RSI_SKILLS_DIR`（默认 `~/.ariacompute/skills`），也就是 `FilesystemSkills`
  扫描、每个 dsh 会话工作区链入的同一个目录（Unix 为 symlink，其他平台复制，目录为空则
  跳过）。运行中会话的系统提示词不会被改写。

### 配置

| 环境变量 | 默认值 | 含义 |
| --- | --- | --- |
| `AGENT_RSI_ENABLED` | `true` | 总开关。设 `false` 时：不入账、不 sweep、无定时器，一轮对话只保留单个持久化 sink |
| `AGENT_RSI_AUTO_PROMOTE` | `false` | 验证通过后无需人工审批即发布（仅限受控部署） |
| `AGENT_RSI_SWEEP_SECS` | `0` | 进程内 sweep 间隔；`0` 表示交给外部 cron 调 `POST /v1/rsi/sweep` |
| `AGENT_RSI_SWEEP_THRESHOLD` | `3` | 累计多少不健康轮次后触发一次 sweep |
| `AGENT_RSI_DOMAIN` | `default` | 进程内定时器 sweep 所用的业务域 |
| `AGENT_RSI_SKILLS_DIR` | `~/.ariacompute/skills` | 共享技能目录：RSI 写入、`FilesystemSkills` 读取、dsh 链入 |
| `AGENT_RSI_REAL_RERUN` | `false` | 真实重跑闸门：每次验证一次无工具模型调用，回执落 `rsi_reruns` |
| `AGENT_RSI_RERUN_TIMEOUT_MS` | `600000` | 单次重跑超时；超时按 `inconclusive` 处理，绝不放行 |

### 兼容性说明

- 仓库外如果直接写字面量构造 `SessionEvent::ItemDone`，需要补上 `latency_ms`
  （通常是 `None`）。线上格式保持向后兼容，只是结构体字面量多了一个字段。
- `RsiConfig::default().enabled` 与 `AGENT_RSI_ENABLED` 的默认值都改为 `true`；
  依赖「不设即关闭」的部署需要显式设 `AGENT_RSI_ENABLED=false`。
- `DshConfig` 新增 `skills` 字段，仓库外的结构体字面量需要补上
  （或用 `DshConfig::for_session(..)` 构造）。

### 验证

`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace`（1150 通过）与 `./scripts/coverage.sh`（计入统计的行
31650/31650 全部覆盖；实测 99.93%，差的 23 行是闭合定界符假象，不含语句）全部通过。

## 统一模型网关配置

`GATEWAY_BASE_URL` / `GATEWAY_API_KEY` / `GATEWAY_MODEL` 现在同时为
`AGENT_HARNESS=dsh` 与 `native` 提供模型配置。独立的
`DEEPSEEK_BASE_URL` / `DEEPSEEK_API_KEY` 已移除，`GATEWAY_*` 也不再回退到
`OPENAI_*`。dsh 会通过其子进程白名单接收这些网关值；设置 `GATEWAY_MODEL` 时，
模型 id 由它决定。

## 受控执行：单一出站边界、审计、计量与凭据

每个出站请求现在都经过同一条边界：判定、替换、记录（ADR-0025）。开启
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
模型密钥（ADR-0025）。

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
