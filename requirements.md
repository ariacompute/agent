# requirements.md — Aria 接入 DeepSeek Harness 与 Pi

> 功能边界 / 配置 / API / 异常 / 验收。本文件为人审规格的落地清单来源。

## 1. 功能边界

### 1.1 范围内
- 共享 TypeScript 桥 `packages/aria-bridge`：engine OpenAI HTTP、memo CLI、环境配置。
- DeepSeek Harness：`dsh/plugins/aria-engine` 注册 LLM adapter（provider 路由 `aria`）；`dsh/plugins/aria-memo` 注册记忆 tools，可选 `autoInject`。
- Pi：`pi/extensions/aria-engine.ts` 以 `createProvider` + `openAICompletionsApi` 注册本地 provider；`pi/extensions/aria-memo.ts` 注册 tools、`/memo-search` slash、可选 `before_agent_start` 注入。
- 加载方式：dsh `--patch dsh/cordis.patch.yml`；`pi install <agent>/pi`。
- 单测：mock `fetch` / spawn / `ctx` / `ExtensionAPI`。无真实权重、无付费 API。

### 1.2 范围外
- sibling `model/`（量化 / 审计 tools / quantize skill）。
- vendor / fork dsh 或 pi 源码。
- 修改 `engine/` `memo/`（不为 memo 加 HTTP/FFI）。
- engine 管理 tools（list / download / serve）。
- FFI / `@ariacompute/engine-ts`。
- 改 agent-loop；向上游提 PR。

## 2. 配置

| 变量 | 默认 | 含义 |
|------|------|------|
| `ARIA_ENGINE_URL` | `http://127.0.0.1:8080/v1` | OpenAI 兼容根路径；无 `/v1` 后缀时自动补上 |
| `ARIA_MEMO_BIN` | `aria-memo` | memo CLI 可执行文件（PATH 或绝对路径） |
| `ARIA_MEMO_DB` | `~/.ariacompute/memo.db` | SQLite 路径 |

dsh plugin Config：`baseUrl`、`model`（engine）；`autoInject`、`topK`（memo）。
Pi 扩展读同一组环境变量；memo `autoInject` 默认 false。

## 3. Engine 接入

- 协议：`GET /v1/models`、`POST /v1/chat/completions`（`stream: true` SSE）。
- 无 API key。请求须带 `User-Agent`（dsh：`attributionHeaders()`；Pi：默认 fetch UA 即可）。
- SSE：解析 `data:` JSON 与 `[DONE]`；honor `AbortSignal`。
- dsh `StreamChunk`：先 `block-start`/`text-delta`/`block-end`，若有 usage 则在 `finish` 之前发出，其后不再发 chunk。
- engine 不可达：抛错，文案含 `aria-engine serve <bundle> --bind 127.0.0.1:8080`。
- Pi：`registerProvider(createProvider({ id: 'aria', api: openAICompletionsApi(), ... }))`；`auth.resolve` 不要求密钥；`fetchModels` 调 `/v1/models`，失败则空 catalog 且错误可观测（不静默当成功）。

## 4. Memo 接入

Tools：`aria_memo_add` / `aria_memo_search` / `aria_memo_get` / `aria_memo_list` / `aria_memo_forget`。

CLI 映射：

| Tool | CLI | 成功 stdout |
|------|-----|-------------|
| add | `add --type --content --importance` | MemoId |
| search | `search --text --top-k` | 行 `score\tcontent`（无 id） |
| get | `get --id` | pretty JSON 或 `not found` |
| list | `list [--type]` | 行 `id [DebugType] content` |
| forget | `forget --id` | `forgotten` / `not found` |

`--type`：`working` / `short_term` / `long_term:episodic|semantic|entity|graph`。
空 content、非法 type、非零退出、缺二进制：失败要响。

autoInject：用当前 user 文本 search，把命中写入下一轮上下文（dsh：`agent/pre-step` 瀑布须 `next()`；Pi：`before_agent_start` 返回 message）。默认关闭。

Pi slash：`/memo-search <query>`。

## 5. 异常

| 情况 | 行为 |
|------|------|
| engine 连接失败 / 非 2xx | `AriaError` `ENGINE_UNREACHABLE` / `ENGINE_HTTP` |
| memo 二进制缺失或 exit≠0 | `AriaError` `MEMO_CLI` |
| 空 content | `AriaError` `EMPTY_CONTENT` |
| 非法参数（top_k=0、importance 越界） | `AriaError` `INVALID_PARAM` |
| get/forget 未找到 | 成功返回 `found: false`，不抛 |

## 6. 验收

- `npm test` 全绿（bridge + dsh mock + pi mock）。
- engine SSE 单测：text →（可选 usage）→ finish，finish 后无 chunk；AbortSignal 中止。
- engine 宕机单测：错误含 serve 提示。
- memo 单测：add/search/get/list/forget 解析；空 content / 非零退出失败。
- dsh：`registerAdapter(['aria'], …)`；memo `register` 五个 tool 名。
- pi：`registerProvider` 一次；memo `registerTool` 五个 + `registerCommand('memo-search')`。
- 文档：加载命令、环境变量、限制（search 无 id；须先 `serve`；不接入 model）。
