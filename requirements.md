# requirements.md — Aria 接入 DeepSeek Harness（dsh）

> 功能边界 / 配置 / API / 异常 / 验收。本文件为人审规格的落地清单来源。
> v2：移除 `pi/` 与独立 `packages/aria-bridge`；仅 dsh；CubeSandbox 沙盒；memo 记忆；隔离持久化工作空间。
> v3：同步 `harness/ariatag` 的 dsh 接入层修正——tool_calls 流翻译（wire index → block index 映射 + canonical `id`）。
> v4：采用 `@ariacompute/engine-ts` SDK（进程内 FFI）取代 `aria-engine serve` HTTP/SSE 接入；新增 `ARIA_MODEL_BUNDLE` + `ARIA_FFI_LIB`。

## 1. 功能边界

### 1.1 范围内
- dsh 插件族（`dsh/plugins/*`，pnpm workspace）：
  - `shared`：engine/memo 环境配置、`AriaError`/`ErrorCode`、spawn 助手（原 `aria-bridge` 代码并入，非独立包）。
  - `aria-engine`：`ctx.llm.registerAdapter(['aria'], …)`，engine OpenAI 兼容 SSE（`GET /v1/models`、`POST /v1/chat/completions` stream）。
  - `aria-memo`：5 个记忆 tools + 可选 `agent/pre-step` `autoInject`（默认关闭）。
  - `aria-sandbox`：CubeSandbox（E2B 兼容）沙盒 tools + 每 agent 一个持久化、相互隔离的工作空间。
- 加载方式：`pnpm dsh web --patch <agent>/dsh/cordis.patch.yml`。
- 单测：mock `fetch` / spawn / `ctx` / 沙盒 factory，不联网、不打真实 CubeAPI。

### 1.2 范围外
- sibling `model/`；vendor/fork dsh；修改 `engine/` `memo/` 源码。
- 非 E2B 沙盒后端（Docker 直跑、VM 直管）。
- 工作空间内容级加密 / 配额 / GC（仅目录 0700 隔离 + 双向同步）。

## 2. 配置

| 变量 | 默认 | 含义 |
|------|------|------|
| `ARIA_MODEL_BUNDLE` | — | model bundle 本地路径（**v4 进程内 FFI 必填**） |
| `ARIA_FFI_LIB` | — | 原生 FFI 动态库路径（`libaria_ffi.so`），供 `@ariacompute/engine-ts` |
| `ARIA_ENGINE_MODEL` | — | in-process 引擎默认模型 id（`adapter.config.defaultModel` 回退） |
| `ARIA_ENGINE_URL` | `http://127.0.0.1:8080/v1` | engine OpenAI 兼容根路径（**v4 已弃用**，仅 HTTP 回退分支保留，in-process adapter 不支持） |
| `ARIA_MEMO_BIN` | `aria-memo` | memo CLI（PATH 或绝对路径） |
| `ARIA_MEMO_DB` | `~/.ariacompute/memo.db` | memo SQLite |
| `E2B_API_URL` | `http://127.0.0.1:3000` | CubeAPI（E2B 兼容） |
| `E2B_API_KEY` | `e2b_000000` | 本地部署任意占位串 |
| `CUBE_TEMPLATE_ID` | — | sandbox-code 模板（`scripts/cube-sandbox-up.sh` 创建） |
| `E2B_TIMEOUT_MS` | `300000` | 沙箱空闲超时（超时 kill） |
| `ARIA_WORKSPACE_ROOT` | `~/.ariacompute/agent/workspaces` | 宿主工作空间根目录 |
| `ARIA_WORKSPACE_SYNC_AFTER_EXEC` | `false` | `sandbox_exec` 后自动 `sync_to_host` |
| `ARIA_WORKSPACE_ID` | — | 默认工作空间 id（工具参数/会话 id 优先） |

dsh plugin Config：engine `bundle`/`ffiLib`/`model`（v4，取代 `baseUrl`）；memo `autoInject`/`topK`；sandbox 同 env 各字段，`workspaceRoot`/`syncAfterExec` 可用 `.env` 覆盖。

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

## 5. Sandbox 接入（CubeSandbox）
- SDK：官方 `e2b` npm SDK 直连 CubeAPI；`Sandbox.create({ apiKey, timeoutMs, template, lifecycle:{onTimeout:'kill'} })`；每 create 一个独立 KVM MicroVM。
- 沙箱内工作目录固定 `/workspace`（envd :49983，模板 expose 49983/49999）。
- 懒加载：首次工具调用才 create；同一工作空间复用同一 Sandbox；插件 `dispose` 时全部 `kill()`。
- 依赖注入：`deps.factory`（默认 `e2bSandboxFactory`）、`deps.registry`（dsh `ctx.workspaceRegistry`，可选）、`deps.workspaceRoot` 均可注入，便于离线单测。

### 5.1 工作空间（每 agent 一个，隔离）
- 工作空间 id 解析优先级：工具参数 `workspace` → 执行上下文 `sessionId` → 插件配置 → `default`；`normalizeWorkspaceId` 只保留 `[A-Za-z0-9._-]`，其余折叠为 `-`。
- 宿主目录：`ARIA_WORKSPACE_ROOT/<workspaceId>/`，`mkdir 0700`，互不重叠。
- 注册：`ctx.workspaceRegistry.create(fs.realpath(dir), title=workspaceId)` 持久化记录；registry 缺失/重复注册静默回退纯目录。
- 隔离三层：
  1. 沙箱层：每工作空间独立 KVM MicroVM（CubeSandbox 硬件隔离）；
  2. 宿主层：目录互不重叠 + `0700`；
  3. 工具层：`assertSandboxPath` 仅允许 `/workspace` 下绝对路径、拒绝 `..` 与越界；`syncToHost` 校验相对路径不逃逸。
- 同步：`sync_to_host`（沙箱→宿主）、`sync_from_host`（宿主→沙箱，沙箱重建后恢复现场）、`workspace_status`（文件数/字节）。

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

## 7. 验收
- `npm test` 全绿（shared + aria-engine + aria-memo + aria-sandbox，均为离线 mock）。
- `npm run typecheck` 全绿。
- sandbox 单测：7 工具注册；sessionId/显式 workspace 归因；同 id 复用沙箱、异 id 隔离；路径逃逸拒绝；create 失败包装 `SANDBOX`；registry 注册一次且容错；`syncAfterExec` 自动拉回；dispose kill；sync 双向 round-trip 与逃逸拒绝。
- v3 tool-call 单测：`parseSseBody` 按 wire index 累积 tool-call（arguments 跨 delta 拼接）；文本+工具混合事件顺序；`chatStream` 请求体 `tools` 透传；`toDshChunks` 文本+工具混合时工具块 index=1（不冲突）；仅工具时工具块 index=0 且 `block-end` 为 canonical `id`；`openaiMessagesFrom` assistant tool-call / tool-result（含 `isError`）序列化；`serializeTools` 转换与空输入返回 undefined；端到端 SSE tool_calls → dsh 工具块。
- 文档：配置表、`scripts/cube-sandbox-up.sh` 用法、限制（search 无 id；须先 `serve`；不接入 model；KVM 要求）。
