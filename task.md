# task.md — Aria 接入 dsh（v4：engine 接入改为 @ariacompute/engine-ts 进程内 FFI）

> 由 `requirements.md`（v4）生成。验收基线：`npm test` 全绿 + `npm run typecheck` 全绿。

1. [x] v1 交付：AGENTS.md + requirements.md + task.md + 双语 README；`packages/aria-bridge`；dsh aria-engine/aria-memo；pi 扩展
2. [x] v2 迁移：删除 `pi/` 与 `packages/aria-bridge/`；共享代码并入 `dsh/plugins/shared/`（config/error/spawn + 平移单测）；`engine.ts` 并入 aria-engine；`memo.ts`/`spawn.ts` 并入 aria-memo
3. [x] v2 工程：pnpm workspace 仅 `dsh/plugins/*`；根 `npm test`/`typecheck` 指向新布局
4. [x] v2 沙盒：`dsh/plugins/aria-sandbox`（e2b SDK → CubeAPI；懒加载/超时/dispose kill；7 tools；依赖注入离线单测）
5. [x] v2 工作空间：每 agent 一个 `ARIA_WORKSPACE_ROOT/<workspaceId>/`（0700）；`workspaceRegistry` 注册（缺失静默回退）；三层隔离（KVM MicroVM / 宿主目录 / 工具层路径校验）；sync 双向 + `syncAfterExec`
6. [x] v2 部署：`scripts/cube-sandbox-up.sh`（KVM 检查 → online-install → 模板创建 → .env 输出）
7. [x] v2 patch：`cordis.patch.yml` 注册 aria-engine + aria-memo + aria-sandbox
8. [x] v2 文档：AGENTS.md / requirements.md / task.md / README 双语 / dsh README 双语
9. [x] v2 验收：`npm install`（必要时 `https_proxy=127.0.0.1:7897`）+ `npm run typecheck` + `npm test` 全绿；无 `packages/`、`pi/` 残留引用
10. [x] v3 同步 ariatag 修正（1286a13 tool_calls index 映射 / 0301de2 canonical `id`）：`stubs/dsh-llm.ts` StreamChunk 加 `tool-call-delta` 与 tool-call `block-end`
11. [x] v3 engine.ts：`parseSseBody` 解析 `delta.tool_calls`（wire index 累积）→ `tool-call` 事件；`ChatStreamRequest.messages` 放宽 `unknown[]` + `tools` 透传
12. [x] v3 adapter.ts：`toDshChunks` blockStack 顺序分配 index（文本+工具不冲突）；工具块 `block-end` canonical `id`；`openaiMessagesFrom` 历史序列化（assistant tool_calls / tool-result → `role:"tool"`）；`serializeTools` 透传
13. [x] v3 单测：engine（tool-call 累积/混合顺序/tools 透传）+ adapter（index 映射/canonical id/历史序列化/端到端）共 9 个新用例，60 全绿
14. [x] v3 文档：requirements.md 3.1 节 + task.md v3 条目
15. [x] v4 依赖：`@ariacompute/engine-ts@^1.4.9`（koffi 原生 FFI）加入 `dsh/plugins/aria-engine/package.json` + 根 `npm install`
16. [x] v4 config：`shared/src/config.ts` `AriaBridgeConfig` 加 `modelBundle`/`engineFfiLib`/`defaultModel`，`loadConfig` 读 `ARIA_MODEL_BUNDLE`/`ARIA_FFI_LIB`/`ARIA_ENGINE_MODEL`；`ARIA_ENGINE_URL` 保留但 in-process adapter 不再使用
17. [x] v4 engine-binding.ts（新增）：`EngineLike`/`EngineFactory` 契约；`createEngineFactory(sdk)` 注入 SDK（测试用 fake）；`generate()` 调 `complete` + `parseEngineResult`，加载失败→`ENGINE_UNREACHABLE`，complete 失败→`ENGINE`；调用后 `close()`
18. [x] v4 engine.ts：移除 SSE 解析（`chatStream`/`parseSseBody`/`listModels` HTTP/`ENGINE_SERVE_HINT`），新增 `parseEngineResult(json)`；保留 `EngineStreamEvent`/`openaiMessagesFrom`/`serializeTools`
19. [x] v4 adapter.ts：`AriaAdapterConfig` 改 `{bundlePath, ffiLib?, defaultModel?, engineFactory?}`；`stream()` 经 `generate` 取事件 + `toDshChunks`，保留 v3 翻译；`listModels`/`resolveModel` 返回 `defaultModel`（无则报错）
20. [x] v4 index.ts：`apply` 读 `modelBundle`/`engineFfiLib`，构造 in-process adapter；无 bundle 则明确抛出（不再回退 HTTP）；cordis.patch.yml 配置改 `bundle`/`ffiLib`/`model`
21. [x] v4 单测：新增 `engine-binding.test.ts`（fake Engine：parseEngineResult 解析/异常映射/close/工厂）；`engine.test.ts` 重写为 parseEngineResult + 序列化 + 异常；`adapter.test.ts` 改用 fake Engine（注册/无 bundle 报错/出流/拒绝 stop/需 model/工具块 canonical id）；丢弃 SSE/fetch mock；66 全绿
22. [x] v4 文档：requirements.md v4（§3 进程内 FFI + 配置表）+ AGENTS.md + README 双语 + dsh/README 双语（移除 serve、加 FFI 配置）
23. [x] v4 验收：`npm run typecheck` + `npm test`（66/66）全绿；无 SSE/fetch 残留引用
