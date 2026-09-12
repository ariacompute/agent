# requirements.md — Aria 接入 DeepSeek Harness（dsh）

> 功能边界 / 配置 / API / 异常 / 验收。本文件为人审规格的落地清单来源。
> v2：移除 `pi/` 与独立 `packages/aria-bridge`；仅 dsh；CubeSandbox 沙盒；memo 记忆；隔离持久化工作空间。
> v3：同步 `harness/ariatag` 的 dsh 接入层修正——tool_calls 流翻译（wire index → block index 映射 + canonical `id`）。
> v4：采用 `@ariacompute/engine-ts` SDK（进程内 FFI）取代 `aria-engine serve` HTTP/SSE 接入；新增 `ARIA_MODEL_BUNDLE` + `ARIA_FFI_LIB`。
> v5：工具链全量迁移到 **Bun v1.4**（`bun install` / `bun test` / 原生 `bun:test` / `bunx tsc` typecheck；`bun.lock` 取代 pnpm-lock）；测试由 `node:test`+`node:assert` 改写为 `bun:test`+`expect`。
> v6：新增 `dsh/plugins/aria-reef`——Reef 式「持续自我改进 agent」闭环 **Serve → Observe → Grow → Commit → Surface**；既演化 harness 构件（技能/提示词/规则，本地 engine FFI），也经 ariapin 派发权重训练（Slime + SGLang）。

## 1. 功能边界

### 1.1 范围内
- dsh 插件族（`dsh/plugins/*`，Bun workspace）：
  - `shared`：engine/memo 环境配置、`AriaError`/`ErrorCode`、spawn 助手（原 `aria-bridge` 代码并入，非独立包）。
  - `aria-engine`：`ctx.llm.registerAdapter(['aria'], …)`，engine OpenAI 兼容 SSE（`GET /v1/models`、`POST /v1/chat/completions` stream）。
  - `aria-memo`：5 个记忆 tools + 可选 `agent/pre-step` `autoInject`（默认关闭）。
  - `aria-sandbox`：可切换沙盒后端（docker / kata / cubesandbox）的沙盒 tools + 每 agent 一个持久化、相互隔离的工作空间。
  - `aria-reef`（v6）：持续自我改进闭环——记录回合 / 三路反馈 / recipe 演化 / 评估选胜者 / Git 版本化 / 热交付（**默认关闭**，见 §7）。
- 加载方式：`bun dsh web --patch <agent>/dsh/cordis.patch.yml`（dsh 经由 Bun/Node 运行）。
- 单测：mock `fetch` / spawn / `ctx` / 沙盒 factory / LLM proposer / git，不联网、不打真实 CubeAPI、不跑真实权重。

### 1.2 范围外
- sibling `model/`；vendor/fork dsh；修改 `engine/` `memo/` 源码。
- 其它沙盒后端（除 docker / kata / cubesandbox 以外的沙盒 provider）。
- 工作空间内容级加密 / 配额 / GC（仅目录 0700 隔离 + 双向同步）。
- 权重训练后端本身（Slime 训练 / SGLang 推理 / ariapin Go 服务）：**本仓库只做 TS 编排 + HTTP 客户端 + 离线 fake**，缺接口在需求/任务清单标注待办，不改 Go 源码。
- Git 远端推送 / PR：reef 只写入本地专属 artifact 仓库（`ARIA_REEF_ARTIFACT_REPO`），不触碰主仓。

## 2. 配置

| 变量 | 默认 | 含义 |
|------|------|------|
| `ARIA_MODEL_BUNDLE` | — | model bundle 本地路径（**v4 进程内 FFI 必填**） |
| `ARIA_FFI_LIB` | — | 原生 FFI 动态库路径（`libaria_ffi.so`），供 `@ariacompute/engine-ts` |
| `ARIA_ENGINE_MODEL` | — | in-process 引擎默认模型 id（`adapter.config.defaultModel` 回退） |
| `ARIA_ENGINE_URL` | `http://127.0.0.1:8080/v1` | engine OpenAI 兼容根路径（**v4 已弃用**，仅 HTTP 回退分支保留，in-process adapter 不支持） |
| `ARIA_MEMO_BIN` | `aria-memo` | memo CLI（PATH 或绝对路径） |
| `ARIA_MEMO_DB` | `~/.ariacompute/memo.db` | memo SQLite |
| `E2B_API_URL` | `http://127.0.0.1:3000` | CubeAPI（E2B 兼容）；仅 cubesandbox 后端使用 |
| `E2B_API_KEY` | `e2b_000000` | 本地部署任意占位串；仅 cubesandbox 后端使用 |
| `CUBE_TEMPLATE_ID` | — | sandbox-code 模板（`scripts/cube-sandbox-up.sh` 创建）；仅 cubesandbox 后端使用 |
| `E2B_TIMEOUT_MS` | `300000` | 沙箱空闲超时（超时 kill）；仅 cubesandbox 后端使用 |
| `ARIA_SANDBOX_TYPE` | `docker` | 沙盒后端：`docker` / `kata` / `cubesandbox`；非法值报错 |
| `ARIA_SANDBOX_IMAGE` | `ubuntu:22.04` | docker/kata 容器镜像 |
| `ARIA_SANDBOX_CLI` | `docker` | OCI CLI 二进制：`docker` 或 `nerdctl` |
| `ARIA_SANDBOX_RUNTIME` | kata 时 `kata` | 容器运行时；kata 后端默认 `kata`，其它后端留空 |
| `ARIA_WORKSPACE_ROOT` | `~/.ariacompute/agent/workspaces` | 宿主工作空间根目录 |
| `ARIA_WORKSPACE_SYNC_AFTER_EXEC` | `false` | `sandbox_exec` 后自动 `sync_to_host` |
| `ARIA_WORKSPACE_ID` | — | 默认工作空间 id（工具参数/会话 id 优先） |
| `ARIA_REEF_ENABLED` | `off` | reef 总开关；`off` 时插件不注册任何 hook/tool |
| `ARIA_REEF_RELEASE` | `dev` | 当前 release id，打在每条 record 上（回滚/对照用） |
| `ARIA_REEF_STORE_DIR` | `~/.ariacompute/agent/reef` | JSONL 存储目录（`records.jsonl` / `feedback.jsonl`） |
| `ARIA_REEF_ARTIFACT_REPO` | `~/.ariacompute/agent/reef-artifacts` | 演化产物的本地 Git 仓库（权重走 Git LFS） |
| `ARIA_REEF_BATCH_SIZE` | `16` | 每轮 Grow 消费的最大 record 数 |
| `ARIA_REEF_MIN_FEEDBACK` | `1` | record 进入训练所需的最少反馈条数 |
| `ARIA_REEF_RUBRIC` | `on` | 自动评分（确定性 rubric，不烧 LLM）开关 |
| `ARIA_REEF_AUTO_APPLY` | `off` | 候选胜出后是否自动发布（关闭时仅产出候选待审） |
| `ARIA_REEF_RECIPES` | `skillclaw,prompt,rules` | 启用的 recipe（加 `weight` 需同时给 base model） |
| `ARIA_REEF_ARIAPIN_URL` | `http://127.0.0.1:8001` | ariapin 服务根地址（权重 recipe） |
| `ARIA_REEF_ARIAPIN_KEY` | — | ariapin API key（`Authorization: Bearer`） |
| `ARIA_REEF_ARIAPIN_TIMEOUT_MS` | `30000` | ariapin 单次 HTTP 超时 |
| `ARIA_REEF_BASE_MODEL` | — | 权重训练 base model（也可 `config.weight.baseModel`） |
| `ARIA_REEF_BUNDLE` | 回退 `ARIA_MODEL_BUNDLE` | harness recipe 提议用的本地 bundle |
| `ARIA_REEF_FFI_LIB` | 回退 `ARIA_FFI_LIB` | harness recipe 提议用的 FFI 库 |
| `ARIA_REEF_MODEL` | 回退 `ARIA_ENGINE_MODEL` | 提议用模型 id |
| `ARIA_REEF_CYCLE` | `off` | 定时自动 cycle 开关；`off` 时不创建任何定时器 |
| `ARIA_REEF_CYCLE_INTERVAL_MS` | `900000`（15min） | 距上次运行超过该间隔即触发一轮 |
| `ARIA_REEF_CYCLE_MIN_SIGNALS` | `8` | 新信号（eligible 未训练 record）达到该数提前触发；`0` 关闭阈值触发 |
| `ARIA_REEF_CYCLE_POLL_MS` | `min(interval, 60000)` | tick 探测周期（每 tick 最多两次 store 读，受 `BATCH_SIZE` 限流） |
| `ARIA_REEF_CYCLE_MAX_BACKOFF_MS` | `interval × 4` | 连续失败退避上限 |

dsh plugin Config：engine `bundle`/`ffiLib`/`model`（v4，取代 `baseUrl`）；memo `autoInject`/`topK`；sandbox 同 env 各字段（`type`/`apiUrl`/`apiKey`/`template`/`timeoutMs`/`image`/`cli`/`runtime`/`workspaceRoot`/`syncAfterExec`/`workspaceId`），`workspaceRoot`/`syncAfterExec` 可用 `.env` 覆盖；reef 同 env 各字段 + `weight.{baseModel,jobType,minRollouts,minScore,lora,hyperparams,agentId}` + `targets.{skill,prompt,rules}` + `cycle*` 各字段。

## 3. Engine 接入（v4：进程内 FFI SDK）
- 采用 `@ariacompute/engine-ts`（`Engine` 类，基于 koffi 原生 FFI）：`new Engine(bundlePath)` + `engine.complete(messages, options, tools)`，进程内加载，无 HTTP、无 SSE。
- `engine-binding.ts` 封装生命周期：`createEngineFactory(sdk)` 注入 SDK（测试用 fake）；`generate()` 调 `complete` 并把返回 JSON 经 `parseEngineResult` 归一化为 `EngineStreamEvent[]`；加载失败 → `AriaError ENGINE_UNREACHABLE`，`complete` 失败 → `AriaError ENGINE`。
- `parseEngineResult`：取 `choices[0].message.{content, tool_calls}` + `usage` + `finish_reason` → text/tool-call/usage/finish 事件。
- dsh `StreamChunk`：`block-start`→`text-delta`→`block-end`→（可选 `usage`）→`finish`，finish 后无 chunk。
- 模型：`listModels`/`resolveModel` 在 in-process 模式下返回 `config.defaultModel`（SDK 无 `/models` 端点；无默认模型则报错而非静默）。
- 无 bundle 配置（`ARIA_MODEL_BUNDLE` 与 config.bundle 均缺）时插件注册失败并明确报错（不再回退 HTTP）。

### 3.1 Tool-call 流翻译（v3 成果，v4 保留）
- `toDshChunks`：文本与工具块共用 dsh block index 空间，**按到达顺序 `blockStack.length` 分配**——OpenAI wire index 不是 block index（文本先出现时工具块 index 必为 1，否则会被文本块覆盖而静默丢失）。
- 工具块结束 `block-end` 使用 **canonical `id`** 字段（不是 `callId`）：dsh-session 会丢弃无 `id` 的 tool-call 块，导致工具永不执行。
- 历史序列化 `openaiMessagesFrom`：assistant `tool-call` 块 → OpenAI `tool_calls`（arguments 对象 JSON 化）；user 单 `tool-result` 块 → `role:"tool"` + `tool_call_id`（`isError` → `is_error:true`）。
- `tools` 透传：dsh tools → OpenAI function tools（`{type:"function", function:{name, description, parameters}}`），请求体 `tools` 仅在非空时发送。

## 4. Memo 接入
- Tools：`aria_memo_add` / `aria_memo_search` / `aria_memo_get` / `aria_memo_list` / `aria_memo_forget`。
- CLI 映射与校验同 v1（`--type` 枚举、`score\tcontent` 无 id、空 content/非法 type/非零退出失败要响）。
- autoInject 默认关闭；开启时 `agent/pre-step` 瀑布必须 `next()`，命中写入下一轮上下文。

## 5. Sandbox 接入（docker / kata / cubesandbox）
- 后端由 `ARIA_SANDBOX_TYPE` 切换（默认 `docker`）：`docker` / `kata` 共用基于 OCI CLI（`docker`/`nerdctl`）的容器客户端；`cubesandbox` 走原 e2b/CubeAPI 路径。插件 `apply` 经 `getSandboxFactory` 选择工厂；`deps.factory` 注入优先（测试用），`cubesandbox` 用 `e2bSandboxFactory`，`docker`/`kata` 用 `createContainerSandboxFactory`。
- docker/kata 容器后端：`create` 时 `cli create --rm=false [--runtime=R] -v <hostDir>:/workspace -w /workspace <image> tail -f /dev/null` 拉起常驻容器并解析 cid；命令走 `cli exec -w <cwd> <cid> sh -c "<command>"`（可选 `timeout` 包裹）；`kill` 走 `cli rm -f <cid>`。文件读写/列举/建目录直接操作宿主 `hostDir`（卷挂载即 `/workspace`，复用 §5.1 的 `/workspace` 路径校验与 0700 隔离），不走 `docker cp`/网络。kata 仅多一个 `--runtime` 标志。
- SDK（cubesandbox）：官方 `e2b` npm SDK 直连 CubeAPI；`Sandbox.create({ apiKey, timeoutMs, template, lifecycle:{onTimeout:'kill'} })`；每 create 一个独立 KVM MicroVM。
- 沙箱内工作目录固定 `/workspace`（envd :49983，模板 expose 49983/49999）。
- 懒加载：首次工具调用才 create；同一工作空间复用同一 Sandbox/容器；插件 `dispose` 时全部 `kill()`。
- 依赖注入：`deps.factory`（默认按 `sandboxType` 选）、`deps.registry`（dsh `ctx.workspaceRegistry`，可选）、`deps.workspaceRoot` 均可注入，便于离线单测。容器命令执行经可注入 `ContainerExecutor`（`run(args)=>{stdout,stderr,exitCode}`），真实实现用 `node:child_process` spawn。

### 5.1 工作空间（每 agent 一个，隔离）
- 工作空间 id 解析优先级：工具参数 `workspace` → 执行上下文 `sessionId` → 插件配置 → `default`；`normalizeWorkspaceId` 只保留 `[A-Za-z0-9._-]`，其余折叠为 `-`。
- 宿主目录：`ARIA_WORKSPACE_ROOT/<workspaceId>/`，`mkdir 0700`，互不重叠。
- 注册：`ctx.workspaceRegistry.create(fs.realpath(dir), title=workspaceId)` 持久化记录；registry 缺失/重复注册静默回退纯目录。
- 隔离三层：
  1. 沙箱层：cubesandbox 每工作空间独立 KVM MicroVM；docker/kata 每工作空间一个独立容器（kata 为 VM 隔离容器，docker 为进程/命名空间隔离）；
  2. 宿主层：目录互不重叠 + `0700`；
  3. 工具层：`assertSandboxPath` 仅允许 `/workspace` 下绝对路径、拒绝 `..` 与越界；`syncToHost` 校验相对路径不逃逸。
- 同步：`sync_to_host`（沙箱→宿主）、`sync_from_host`（宿主→沙箱，沙箱重建后恢复现场）、`workspace_status`（文件数/字节）。**docker/kata 后端因卷挂载实时同步，`sync_to_host`/`sync_from_host` 为 no-op（返回 `{synced:0}`），`syncAfterExec` 对该类后端无意义（忽略）。**

### 5.2 Tools
`sandbox_exec`（默认 cwd `/workspace`）、`sandbox_read_file`、`sandbox_write_file`、`sandbox_list_files`、`sandbox_sync_to_host`、`sandbox_sync_from_host`、`workspace_status`。

## 6. 异常
| 情况 | 行为 |
|------|------|
| engine 连接失败 / 非 2xx | `AriaError` `ENGINE_UNREACHABLE` / `ENGINE_HTTP` |
| memo 二进制缺失 / exit≠0 | `AriaError` `MEMO_CLI` |
| 空 content / 非法参数 | `AriaError` `EMPTY_CONTENT` / `INVALID_PARAM` |
| 沙箱路径越界 / `..` | `AriaError` `INVALID_PARAM` |
| 沙箱 create/操作失败 | `AriaError` `SANDBOX`（含 KVM 等根因） |
| 同步文件逃逸 `/workspace` | `AriaError` `WORKSPACE` |
| get/forget 未找到 | 成功返回，不抛 |
| reef：无 LLM proposer（无 bundle 且未注入 `deps.llm`） | `AriaError` `REEF`，cycle 记 `error` 不中断其它 recipe |
| reef：store 写入失败 / JSONL 损坏行 | `AriaError` `REEF_STORE`（记录落库在 detached promise 内，只记日志不打断 agent 回路） |
| reef：rollout 不足 / 训练任务未成功 | `AriaError` `REEF_TRAIN` |
| reef：git 命令失败（`add`/`commit` 之外） | `AriaError` `REEF_GIT`；`commit` 无变更返回 `null` 并记日志 |
| reef：ariapin 非 2xx / `code != 0` / 无 `job_id` | `AriaError` `ARIAPIN` |
| reef：非法评分 / `structured` 非 JSON 对象 / 未知 `record_id` / 非法 artifact 名 | `AriaError` `INVALID_PARAM` |
| reef：权重重载接口缺失且无 `agentId` | 原样抛 `AriaError` `ARIAPIN`（不静默降级）；有 `agentId` 时回退 agent 重启并在 detail 注明 |
| reef：自动 cycle 抛错 / 信号计数失败 | 只记日志（消息截断）+ 指数退避 / 跳过信号触发，调度器与 agent 主回路不受影响 |

## 7. Reef 自我改进闭环（v6）

### 7.1 阶段与数据流

```
Agent turn ──agent/pre-step──► Serve：签发 record_id（异步落库）
        ──response/tool result──► Observe：三路反馈绑定 record_id
                                    ├─ aria_reef_report（显式：评分/文本/结构化）
                                    ├─ 任务结果信号（post-step 推断 success/failure）
                                    └─ rubric 自动评分（确定性、不烧 LLM）
Records + Feedback ──eligibility 门控──► Grow：recipe 产出 Candidate
                                    ├─ harness（skillclaw / prompt / rules，engine FFI 提议）
                                    └─ weight（聚合 scored rollout → ariapin 派发 Slime 训练）
Candidate ──Evaluate（当前 vs 候选，保留胜者）──► Commit（Git + LFS）
                                    └──► Surface（harness 热更新 / 权重热重载）
```

- `record_id` 以 `reef.recordId` 形式挂在 `agent/pre-step` 载荷上回传给 agent；落库为 detached promise，**失败只记日志，绝不 `await` 后阻塞 `next()`**。
- 反馈三路统一为 `Feedback{recordId, source, score(0..1), text?, structured?, eligible}`；`1..5` 分制自动归一化（`/5`）。
- eligibility：未消费（`trainedAt` 为空）+ 反馈数 >= `minFeedback` + 至少一条含 score。
- 消费语义：recipe 一旦产出候选即标记 `trainedAt`，失败不会在同批数据上死循环。

### 7.2 Recipes

| recipe | kind | artifact | 触发条件 | 产物 |
|--------|------|----------|----------|------|
| `skillclaw` | harness | `skill` (`skills/agent`) | 样本数 >= `minSamples` 且均分 < `scoreThreshold`（默认 1 / 0.7） | 新的技能文件全文 + unified diff |
| `prompt` | harness | `prompt` (`prompts/system`) | 同上 | 新的系统提示词全文 |
| `rules` | harness | `rules` (`rules/guardrails`) | 同上 | 新的规则/护栏全文 |
| `weight` | weight | `weight` (`weights/active`) | 高分 rollout（>= `minScore`，默认 0.6）数 >= `minRollouts`（默认 4） | ariapin dataset + job，候选内容为训练任务引用 |

- harness recipe 经 `LlmProposer` 提议：默认走 `@ariacompute/engine-ts` 进程内 FFI（**动态 import**，未安装/无 bundle 时给出明确错误，不静默跳过；测试注入 `fakeProposer`）。
- weight recipe 只写 TS 编排：`POST /v1/datasets`（multipart）→ `POST /v1/jobs` → `POST /v1/jobs/{id}/start`，可选 `waitForJob` 轮询到终态。
- **权重重载（v6+ 已补齐后端）**：ariapin 提供 `POST /v1/models/{id}/reload`（模型 → `job_id` → `jobs.agent_id` 解析后重启承载 agent，`202` + `mode:"restart"`）。reef 优先调用模型重载；失败且已知 `agentId` 时回退 `POST /v1/agents/{id}/restart` 并在 detail 注明；两者皆不可用则原样抛 `AriaError(ARIAPIN)`（失败要响，不静默）。`agentId` 由 `config.weight.agentId` 写入候选 meta。

### 7.3 评估与版本化

- 评估：`tasksFromRecords` 由反馈文本抽关键词作 hints，默认 runner 计算「非空的 0.5 基础分 + hints 覆盖率的 0.5」；`candidate > current + minImprovement` 才取代（**平局保留当前版本**）。runner 可注入（在线可换成真实任务/LM 打分）。
- 版本化：artifact 仓库布局 `artifacts/<kind>/<name>`（active）、`history/<kind>/<name>/vN`（快照）、`reef-state.json`（版本指针）、`.gitattributes`（`*.bin/*.safetensors/*.gguf/*.pt` 走 LFS）。
- 每次发布：`write` 递增版本 → `git add -A` → `git commit` → `rev-parse HEAD` 记 sha；无变更返回 `null`。
- `git lfs install --local` 失败仅记日志（权重退化为直接入库），不阻断。

### 7.4 交付（Surface）

- harness 构件：`surface.active(ref)` **每回合实时读盘**，不做进程内缓存 → 被接受的技能/提示词/规则改动无需重启即生效。
- 权重：候选 `meta.modelId`/`agentId` 存在时经 ariapin 触发热重载；无客户端则明确降级并记日志。
- `ARIA_REEF_AUTO_APPLY=off` 时只产出候选与评估报告，等人工/上层调用 `aria_reef_cycle` 决策。

### 7.5 Tools

| tool | 说明 |
|------|------|
| `aria_reef_report` | 显式反馈：`record_id`（缺省取最近一次回合）、`score`（0..1 或 1..5）、`text`、`structured`（JSON 字符串） |
| `aria_reef_status` | 观测：release、record 总数、待消费数、反馈数、启用 recipes |
| `aria_reef_cycle` | 手动触发一轮 Grow→Evaluate→Commit→Surface，返回 `CycleReport` |

### 7.6 爆炸半径

- `aria-engine` / `aria-memo` / `aria-sandbox` 零改动；`aria-reef` 只依赖 `shared`（新增 `REEF*` / `ARIAPIN` 错误码为纯增量）。
- 默认 `ARIA_REEF_ENABLED=off`：`apply` 直接返回，不注册 hook/tool，不建目录。
- Git 操作仅限 `ARIA_REEF_ARTIFACT_REPO`；artifact 名强制 `[A-Za-z0-9._-/]` 且拒绝 `..`。

## 8. 验收
- `bun test` 全绿（shared + aria-engine + aria-memo + aria-sandbox + aria-reef，均为离线 mock）。
- `bunx tsc --noEmit` 各插件 typecheck 全绿（根 `bun run typecheck`）。
- sandbox 单测：7 工具注册；sessionId/显式 workspace 归因；同 id 复用沙箱、异 id 隔离；路径逃逸拒绝；create 失败包装 `SANDBOX`；registry 注册一次且容错；`syncAfterExec` 自动拉回（cubesandbox）；dispose kill；sync 双向 round-trip 与逃逸拒绝；`ARIA_SANDBOX_TYPE` 默认 `docker`、非法值报错；容器后端 create 参数含卷挂载与 kata runtime、`exec` 返回 stdout/stderr/exitCode、文件走宿主 FS、kill 发 `rm -f`；容器后端 `sync_*` 退化为 no-op。
- v3 tool-call 单测：`parseSseBody` 按 wire index 累积 tool-call（arguments 跨 delta 拼接）；文本+工具混合事件顺序；`chatStream` 请求体 `tools` 透传；`toDshChunks` 文本+工具混合时工具块 index=1（不冲突）；仅工具时工具块 index=0 且 `block-end` 为 canonical `id`；`openaiMessagesFrom` assistant tool-call / tool-result（含 `isError`）序列化；`serializeTools` 转换与空输入返回 undefined；端到端 SSE tool_calls → dsh 工具块。
- 文档：配置表、`scripts/cube-sandbox-up.sh` 用法、限制（search 无 id；须先 `serve`；不接入 model；KVM 要求）。
- v6 reef 单测（85 例，全离线）：config 环境变量与非法值回退；store 内存/文件 JSONL 往返 + 损坏行 `REEF_STORE`；serve hook 必 `next()`、持久化失败不打断、post-step 补全 outcome；反馈归一化（1..5）/三路来源/eligibility/rubric 幂等/`aria_reef_report` 四类参数校验/`aria_reef_status` 计数；评估 runner hints 覆盖、平局保留当前、runner 非有限值与抛错映射；recipe 门控、diff 生成、registry 选择；weight rollout 门槛、dataset+job 派发、`waitForJob` 失败码；artifact 版本递增/历史快照/LFS 属性/git 失败码/commit 无变更 `null`；surface 热更新读盘与降级；ariapin 客户端 multipart/契约键/终态轮询/超时/三类错误映射；插件：默认关闭零注册、启用注册 3 工具、完整 cycle 落版本并标记消费、`autoApply=off` 不发布、recipe 失败记 `error` 不中断。
- v6 验收命令：`bun test`（含 `dsh/plugins/aria-reef/test/*.test.ts`）+ `bun run typecheck`（含 aria-reef）全绿。
- v6+ 定时自动 cycle 单测（离线假时钟，不真实睡眠）：关闭时零定时器（`setTimer` 未被调用）；间隔未满不跑、满间隔跑一轮；信号达阈值提前触发（受 60s gate 约束）、未达阈值不触发；在飞时 tick 跳过并计 `skipped`；连续失败按 `pollMs → 2× → …` 退避且封顶、成功归零、调度器不中断；`stop()` 清定时器且等在飞 cycle；信号计数失败只记日志不影响间隔触发；插件 `dispose` 后调度器 `started=false`。
- v6+ 权重重载：`POST /v1/models/{id}/reload` 成功路径；模型重载失败且有 `agentId` 时回退 `POST /v1/agents/{id}/restart`（detail 含 `fell back`）；无 `agentId` 时原样抛 `ARIAPIN`；weight recipe 写入 `meta.agentId`。
- v6+ ariapin（harness 仓库）验收：`go build ./... && go vet ./... && go test ./...` 全绿 + `python3 -m unittest discover -s tests -p "test_*.py"` 全绿；`ResolveReloadTarget` 纯函数覆盖正常与三类 422 + nil model 404。
