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
24. [x] v5 工具链：Bun v1.4 全量化——删除 `pnpm-workspace.yaml` 与 `pnpm-lock.yaml`/`package-lock.json`，改用根 `package.json` `workspaces` 字段 + `bun.lock`；根 `package.json` 脚本 `test`→`bun test`、`typecheck`→`bunx tsc --noEmit`（保留 `tsc` 严格门禁）、新增 `build`（`bun build --compile` 单文件可执行）；`engines` 加 `bun>=1.4.0`、移除 `tsx` devDep；装 `@types/bun`
25. [x] v5 测试：10 个 `*.test.ts` 从 `node:test`+`node:assert` 改写为 `bun:test`（`describe`/`it`/`expect`），断言等价迁移（`equal`→`toBe`、`deepEqual`→`toEqual`、`throws`→`toThrow`/属性再断言）；Bun 的 `after` 不存在→改用 `afterAll`
26. [x] v5 tsconfig：`tsconfig.base.json` 保留 `NodeNext` + `allowImportingTsExtensions` + `verbatimModuleSyntax`（Bun 原生支持 `.ts` 导入）；`types` 加 `bun` 以解析 `bun:test`
27. [x] v5 验收：`bun install` 生成 `bun.lock` 且无 pnpm/package 锁残留；`bun test` 66/66 全绿；`bunx tsc --noEmit` 各插件全绿；文档命令统一为 bun
28. [x] v6 骨架：新增 `dsh/plugins/aria-reef`（package.json / tsconfig.json / `src/{index,config,types,store,diff}.ts`）；`shared/src/error.ts` 增量加 `REEF`/`REEF_STORE`/`REEF_TRAIN`/`REEF_GIT`/`ARIAPIN` 错误码；依赖仅指向 `shared`（不引用其它插件）
29. [x] v6 Serve：`src/record.ts`——`agent/pre-step` 签发 `record_id`（`reef.recordId` 回传）+ detached 落库（失败只记日志，必调 `next()`）；`agent/post-step` 推断 outcome 并补全 record；`promptOf`/`outcomeFromResponse`/`attachReceipt` 可单测
30. [x] v6 Observe：`src/feedback.ts`——`aria_reef_report`（score 0..1/1..5 归一化 + text + structured）+ 任务结果信号 + 确定性 rubric 自动评分（`applyRubric` 幂等）+ `isEligible` 门控；`aria_reef_status` 计数器
31. [x] v6 存储：`src/store.ts`——`RecordStore`/`FeedbackStore` 抽象 + 内存实现（离线/单测）+ JSONL 文件实现（原子重写，损坏行 `REEF_STORE` 失败要响）
32. [x] v6 Grow/harness：`src/recipes/{recipe,llm,skillclaw,prompt,rules}.ts`——统一 `Recipe` 契约与 registry；`LlmProposer` 复用 `EngineLike`/`EngineFactory` 范式（本地重声明以 decoupling），engine-ts 动态 import（未安装/无 bundle 明确报错）；候选产出全文 + unified diff
33. [x] v6 Grow/weight：`src/ariapin-client.ts`（`fetch` 可注入，multipart dataset + `/v1/jobs` 契约键 + 终态轮询/超时 + 错误映射 + 离线 fake）+ `src/recipes/weight.ts`（scored rollout 聚合 → dataset/job 派发）
34. [x] v6 Evaluate/Commit：`src/evaluate.ts`（hints runner、平局保留当前、`minImprovement`）+ `src/train.ts`（batch + eligibility + 逐 recipe 报告 + 消费标记）+ `src/artifact.ts`（版本递增、历史快照、LFS `.gitattributes`、`git` 经 `RunCommand` 注入）
35. [x] v6 Surface：`src/surface.ts`——`active()` 每回合实时读盘实现 harness 构件热更新（无重启）；权重经 ariapin `POST /v1/models/{id}/reload`（或 agent restart）热重载，缺客户端明确降级
36. [x] v6 接线：`dsh/cordis.patch.yml` 增加 `aria-reef` insert（enabled/recipe/ariapin/weight 配置）；根 `package.json` `test`/`typecheck` 脚本纳入 aria-reef
37. [x] v6 单测：`test/{config,store,record,feedback,evaluate,recipes,weight,artifact,surface,ariapin-client,plugin}.test.ts` —— 85 例全绿（正常 + 异常路径，全 mock/注入，不联网、不打真实权重）
38. [x] v6 文档：requirements.md §7（Reef 闭环）+ 配置表 + 异常表 + 验收；AGENTS.md / README.md / README_cn.md 双语同步
39. [x] v6+ ariapin 权重重载接口（harness/ariapin）：`internal/models/reload.go`（纯函数 `ResolveReloadTarget` 覆盖「无源任务/源任务不存在/未绑定 agent」三类 422 + DB 版 `(*Service).ReloadTarget`）+ `POST /v1/models/:id/reload`（`authed` 组，复用 `agentSvc.Restart`，`202` + `mode:"restart"`）+ Python `reload_model` + `reload_test.go` 离线单测；文档同步 `requirements.md`（F26/端点表/E23/A49）与 `task.md` M15；`go build/vet/test` 全绿
40. [x] v6+ 定时自动 cycle：`src/scheduler.ts`（间隔 OR 信号阈值双触发、单飞互斥计数 `skipped`、detached 执行不进主回路、失败指数退避上限、成功归零、`stop()` 清定时器并等在飞）+ config 新增 `ARIA_REEF_CYCLE`/`_CYCLE_INTERVAL_MS`/`_CYCLE_MIN_SIGNALS`/`_CYCLE_POLL_MS`/`_CYCLE_MAX_BACKOFF_MS`；`index.ts` 接线（`autoCycle` 关闭零定时器、dispose 先 `scheduler.stop()` 再 `recorder.flush()`、`ReefPlugin.scheduler` 暴露、`aria_reef_status` 条件追加 `autoCycle`）
41. [x] v6+ 健壮性：`ariapin-client.reloadModel` 在模型重载失败且提供 `agentId` 时回退 `POST /v1/agents/{id}/restart`（detail 注明回退，两者皆无则原样抛 `AriaError(ARIAPIN)`）；weight recipe 支持可选 `agentId` 写入候选 meta
42. [x] v6+ 单测：`test/scheduler.test.ts`（8 例：间隔触发/信号阈值/阈值不足/单飞跳过/退避+恢复/stop 清定时器并在飞/计数失败容错）+ `helpers.ts` 新增 `createFakeTimers()` 假时钟（不真实睡眠）+ config 新字段 + 插件（关闭零定时器、开启后 dispose 停止）+ 客户端回退与 weight agentId；`bun test` 177 全绿、`bunx tsc --noEmit` 全绿
43. [ ] v6 后端待办（不阻塞本期）：真实任务/LM 评估 runner（当前 `evaluate.ts` 为确定性 hints runner）
