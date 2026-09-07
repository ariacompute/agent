# AGENTS.md — aria agent

工程上下文入口。先看概述/架构/目录，动手时再看规范/命令/进行中/注意。

## 概述
`agent` 仓库 = Aria 对 **DeepSeek Harness (`dsh`)** 的 out-of-tree 接入层。
不 fork 上游。**engine** 作 LLM 后端（`@ariacompute/engine-ts` 进程内 FFI，取代 `aria-engine serve`）；**memo** 作 agent context memory（tools + 可选注入）；
**CubeSandbox**（E2B 兼容）作 agent 沙盒，每 agent 一个持久化且相互隔离的工作空间。
**aria-reef** 提供 Reef 式「持续自我改进」闭环（Serve → Observe → Grow → Commit → Surface）。
本里程碑不接入 sibling `model/`。已移除 `pi/` 与独立 `packages/aria-bridge/`。

## 架构
`dsh/plugins/*`（Cordis plugin，Bun workspace）：
- `shared`：config / `AriaError` / spawn（原 aria-bridge 并入，非独立包）
- `aria-engine`：`ctx.llm.registerAdapter(['aria'], …)` → `Engine` 进程内 FFI（`@ariacompute/engine-ts`，bundle + `libaria_ffi.so`）
- `aria-memo`：tools `aria_memo_*` + 可选 `agent/pre-step` autoInject（默认关闭）
- `aria-sandbox`：e2b SDK 直连 CubeAPI；tools `sandbox_*` + `workspace_status`；隔离持久化工作空间
- `aria-reef`（默认关闭）：
  - **Serve** `record.ts`：`agent/pre-step` 签发 `record_id` 并异步落库（detached，失败只记日志）
  - **Observe** `feedback.ts`：`aria_reef_report` + 任务结果信号 + 确定性 rubric + eligibility 门控
  - **Grow** `recipes/*`：`skillclaw`/`prompt`/`rules`（engine FFI 提议编辑）+ `weight`（ariapin 派发训练）
  - **Commit** `artifact.ts`/`evaluate.ts`/`train.ts`：当前 vs 候选保留胜者 → Git 版本化（权重 LFS）
  - **Surface** `surface.ts`：harness 构件每回合实时读盘（热更新无重启）+ 权重经 ariapin 热重载
  - **自动 cycle** `scheduler.ts`（默认关闭）：间隔 OR 信号阈值双触发、单飞、detached、退避、优雅停止

依赖单向：插件依赖 `shared`，不依赖彼此（`aria-reef` 本地重声明 `EngineLike`/`EngineFactory` 以免跨插件耦合）。`scripts/cube-sandbox-up.sh` 一键部署沙盒。

## 目录
- `dsh/plugins/shared|aria-engine|aria-memo|aria-sandbox|aria-reef`：插件源码 + `test/`
- `dsh/cordis.patch.yml`：`bun dsh web --patch` 加载清单（含 aria-sandbox）
- `dsh/stubs/`：cordis/dsh-llm/dsh-tools 最小类型 stub（仅 typecheck/单测）
- `scripts/`：CubeSandbox 一键引导
- 根：`AGENTS.md` / `requirements.md` / `task.md` / `README.md` / `README_cn.md`

## 开发规范
- TypeScript ESM；失败要响，禁止静默 skip。
- 新增功能同步单测（正常 + 异常）；不跑真实权重、不打付费 LLM、不连真实沙盒。
- 不改 `engine/` `memo/` 源码；不 vendor dsh。
- 沙盒接入必须可注入（`deps.factory`/`registry`/`workspaceRoot`）以便离线测试。
- reef 全部外部依赖必须可注入：`deps.store`/`deps.llm`/`deps.ariapin`/`deps.fetch`/`deps.runCommand`；record 落库必须 detached，禁止在 `next()` 前 `await` 重 IO。
- 工作空间隔离是硬约束：`assertSandboxPath` 只放行 `/workspace` 下路径；宿主目录 0700。

## 常用命令
- `bun test` / `bun run typecheck`（typecheck = `bunx tsc --noEmit` 各插件）
- `scripts/cube-sandbox-up.sh`（KVM + cubemastercli + 模板 + `.env`）
- `bun dsh web --patch <agent>/dsh/cordis.patch.yml`

## 进行中需求
Spec 见 `requirements.md`（v6：aria-reef 自我改进闭环；v6+：ariapin 权重重载接口 + 定时自动 cycle），清单见 `task.md`（第 28–43 项）。

## 注意事项
- 黄金路径：设置 `ARIA_MODEL_BUNDLE` + `ARIA_FFI_LIB` → adapter 进程内 `Engine.complete` 出流；memo CLI add → search；CubeSandbox 就绪（`E2B_API_URL`/`CUBE_TEMPLATE_ID`）→ `sandbox_exec`。
- `E2B_API_URL` 默认 `http://127.0.0.1:3000`（CubeAPI）；模板须先建（`cubemastercli tpl create-from-image`）。
- 网络：GitHub/registry 直连失败时 `export https_proxy=http://127.0.0.1:7897`。
- memo search stdout 无 id（`score\tcontent`）；`ARIA_MEMO_DB` 默认 `~/.ariacompute/memo.db`。
- reef 默认 `ARIA_REEF_ENABLED=off`（零注册）；开启后 `record_id` 以 `reef.recordId` 回传，存储 `ARIA_REEF_STORE_DIR`、产物仓库 `ARIA_REEF_ARTIFACT_REPO`（仅本地 Git，不推远端、不动主仓）。
- reef 自动 cycle 默认 `ARIA_REEF_CYCLE=off`（零定时器）；开启后按 `ARIA_REEF_CYCLE_INTERVAL_MS`/`_MIN_SIGNALS` 双触发，失败只记日志 + 指数退避。
- weight recipe 需 ariapin（`ARIA_REEF_ARIAPIN_URL` + key + `ARIA_REEF_BASE_MODEL`）；harness recipe 需 `ARIA_REEF_BUNDLE`（或注入 `deps.llm`）。
- 工作空间根 `ARIA_WORKSPACE_ROOT` 默认 `~/.ariacompute/agent/workspaces`；`syncAfterExec` 默认 false。
- dsh/pi stub 仅供 typecheck/单测；运行时由已安装的 harness 解析真实包。
