# task.md — Aria 接入 dsh（v3：同步 ariatag dsh 接入层修正）

> 由 `requirements.md`（v3）生成。验收基线：`npm test` 全绿 + `npm run typecheck` 全绿。

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
