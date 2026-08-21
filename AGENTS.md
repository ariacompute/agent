# AGENTS.md — aria agent

工程上下文入口。先看概述/架构/目录，动手时再看规范/命令/进行中/注意。

## 概述
`agent` 仓库 = Aria 对 **DeepSeek Harness (`dsh`)** 的 out-of-tree 接入层。
不 fork 上游。**engine** 作 LLM 后端（`@ariacompute/engine-ts` 进程内 FFI，取代 `aria-engine serve`）；**memo** 作 agent context memory（tools + 可选注入）；
**CubeSandbox**（E2B 兼容）作 agent 沙盒，每 agent 一个持久化且相互隔离的工作空间。
本里程碑不接入 sibling `model/`。已移除 `pi/` 与独立 `packages/aria-bridge/`。

## 架构
`dsh/plugins/*`（Cordis plugin，pnpm workspace）：
- `shared`：config / `AriaError` / spawn（原 aria-bridge 并入，非独立包）
- `aria-engine`：`ctx.llm.registerAdapter(['aria'], …)` → `Engine` 进程内 FFI（`@ariacompute/engine-ts`，bundle + `libaria_ffi.so`）
- `aria-memo`：tools `aria_memo_*` + 可选 `agent/pre-step` autoInject（默认关闭）
- `aria-sandbox`：e2b SDK 直连 CubeAPI；tools `sandbox_*` + `workspace_status`；隔离持久化工作空间

依赖单向：插件依赖 `shared`，不依赖彼此。`scripts/cube-sandbox-up.sh` 一键部署沙盒。

## 目录
- `dsh/plugins/shared|aria-engine|aria-memo|aria-sandbox`：插件源码 + `test/`
- `dsh/cordis.patch.yml`：`pnpm dsh web --patch` 加载清单（含 aria-sandbox）
- `dsh/stubs/`：cordis/dsh-llm/dsh-tools 最小类型 stub（仅 typecheck/单测）
- `scripts/`：CubeSandbox 一键引导
- 根：`AGENTS.md` / `requirements.md` / `task.md` / `README.md` / `README_cn.md`

## 开发规范
- TypeScript ESM；失败要响，禁止静默 skip。
- 新增功能同步单测（正常 + 异常）；不跑真实权重、不打付费 LLM、不连真实沙盒。
- 不改 `engine/` `memo/` 源码；不 vendor dsh。
- 沙盒接入必须可注入（`deps.factory`/`registry`/`workspaceRoot`）以便离线测试。
- 工作空间隔离是硬约束：`assertSandboxPath` 只放行 `/workspace` 下路径；宿主目录 0700。

## 常用命令
- `npm test` / `npm run typecheck`
- `scripts/cube-sandbox-up.sh`（KVM + cubemastercli + 模板 + `.env`）
- `pnpm dsh web --patch <agent>/dsh/cordis.patch.yml`

## 进行中需求
Spec 见 `requirements.md`（v4，engine 接入改为 `@ariacompute/engine-ts` 进程内 FFI）。清单见 `task.md`。

## 注意事项
- 黄金路径：设置 `ARIA_ENGINE_BUNDLE` + `ARIA_FFI_LIB` → adapter 进程内 `Engine.complete` 出流；memo CLI add → search；CubeSandbox 就绪（`E2B_API_URL`/`CUBE_TEMPLATE_ID`）→ `sandbox_exec`。
- `E2B_API_URL` 默认 `http://127.0.0.1:3000`（CubeAPI）；模板须先建（`cubemastercli tpl create-from-image`）。
- 网络：GitHub/registry 直连失败时 `export https_proxy=http://127.0.0.1:7897`。
- memo search stdout 无 id（`score\tcontent`）；`ARIA_MEMO_DB` 默认 `~/.ariacompute/memo.db`。
- 工作空间根 `ARIA_WORKSPACE_ROOT` 默认 `~/.ariacompute/agent/workspaces`；`syncAfterExec` 默认 false。
- dsh/pi stub 仅供 typecheck/单测；运行时由已安装的 harness 解析真实包。
