# task.md — Aria 接入 dsh（v2：仅 dsh + CubeSandbox + memo + 隔离工作空间）

> 由 `requirements.md`（v2）生成。验收基线：`npm test` 全绿 + `npm run typecheck` 全绿。

1. [x] v1 交付：AGENTS.md + requirements.md + task.md + 双语 README；`packages/aria-bridge`；dsh aria-engine/aria-memo；pi 扩展
2. [x] v2 迁移：删除 `pi/` 与 `packages/aria-bridge/`；共享代码并入 `dsh/plugins/shared/`（config/error/spawn + 平移单测）；`engine.ts` 并入 aria-engine；`memo.ts`/`spawn.ts` 并入 aria-memo
3. [x] v2 工程：pnpm workspace 仅 `dsh/plugins/*`；根 `npm test`/`typecheck` 指向新布局
4. [x] v2 沙盒：`dsh/plugins/aria-sandbox`（e2b SDK → CubeAPI；懒加载/超时/dispose kill；7 tools；依赖注入离线单测）
5. [x] v2 工作空间：每 agent 一个 `ARIA_WORKSPACE_ROOT/<workspaceId>/`（0700）；`workspaceRegistry` 注册（缺失静默回退）；三层隔离（KVM MicroVM / 宿主目录 / 工具层路径校验）；sync 双向 + `syncAfterExec`
6. [x] v2 部署：`scripts/cube-sandbox-up.sh`（KVM 检查 → online-install → 模板创建 → .env 输出）
7. [x] v2 patch：`cordis.patch.yml` 注册 aria-engine + aria-memo + aria-sandbox
8. [x] v2 文档：AGENTS.md / requirements.md / task.md / README 双语 / dsh README 双语
9. [x] v2 验收：`npm install`（必要时 `https_proxy=127.0.0.1:7897`）+ `npm run typecheck` + `npm test` 全绿；无 `packages/`、`pi/` 残留引用
