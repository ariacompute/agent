# task.md — Aria 接入 dsh / Pi

> 由 `requirements.md` 生成。验收基线：`npm test` 全绿。

1. [x] `AGENTS.md`（≤100 行）+ `requirements.md` + 本清单 + 双语 README
2. [x] `packages/aria-bridge`：config / engine SSE / memo CLI / AriaError + mock 单测
3. [x] 目录 `agent/dsh`（由 `deepseek-harness` 重命名）：aria-engine adapter、aria-memo tools+inject、`cordis.patch.yml`
4. [x] `agent/pi` pi-package：`registerProvider(openAICompletionsApi)`、memo tools、`/memo-search`
5. [x] dsh/pi 插件单测（mock ctx / ExtensionAPI）
6. [x] `npm test` 全绿
