# AGENTS.md — aria agent

工程上下文入口。先看概述/架构/目录，动手时再看规范/命令/进行中/注意。

## 概述
`agent` 仓库 = Aria 对 **DeepSeek Harness (`dsh`)** 与 **Pi** 的 out-of-tree 接入层。
不 fork 上游。**engine** 作为 OpenAI 兼容 LLM 后端；**memo** 作为记忆 tools + 可选上下文注入。
本里程碑不接入 sibling `model/`。

## 架构
`packages/aria-bridge`（spawn + fetch，零 harness 依赖）→ `dsh/plugins/*`（Cordis plugin）
与 `pi/extensions/*`（Pi extension）。依赖单向：插件依赖 bridge，bridge 不依赖插件。

## 目录
- `packages/aria-bridge`：engine OpenAI SSE、memo CLI、配置/错误
- `dsh/`：`plugins/aria-engine`（`ctx.llm.registerAdapter`）、`plugins/aria-memo`（tools + inject）、`cordis.patch.yml`
- `pi/`：pi-package，`extensions/aria-engine.ts`（`registerProvider`）、`extensions/aria-memo.ts`（tools + slash）
- 根：`AGENTS.md` / `requirements.md` / `task.md` / `README.md` / `README_cn.md`

## 开发规范
- TypeScript ESM；失败要响，禁止静默 skip。
- 新增功能同步单测（正常 + 异常）；不跑真实权重、不打付费 LLM。
- 不改 `engine/` `memo/` 源码；不 vendor dsh/pi。
- AGENTS.md ≤100 行；API 细节下沉 `requirements.md`。

## 常用命令
- `npm test` / `npm run typecheck`
- `aria-engine serve <bundle> --bind 127.0.0.1:8080`
- `pnpm dsh web --patch <agent>/dsh/cordis.patch.yml`
- `pi install <agent>/pi`

## 进行中需求
Spec 见 `requirements.md`。清单见 `task.md`。

## 注意事项
- 黄金路径：engine HTTP 可达 → adapter/provider 出流；memo CLI add → search。
- `ARIA_ENGINE_URL` 默认 `http://127.0.0.1:8080/v1`；engine 未启动须报启动命令。
- memo search stdout 无 id（`score\tcontent`）；`ARIA_MEMO_DB` 默认 `~/.ariacompute/memo.db`。
- dsh/pi stub 仅供本仓库 typecheck/单测；运行时由已安装的 harness 解析真实包。
