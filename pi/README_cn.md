# Pi 包

[English](README.md) | [中文](README_cn.md)

从受信任的本地路径安装（[Pi packages](https://pi.dev/docs/latest/packages)）：

```sh
aria-engine serve <bundle> --bind 127.0.0.1:8080
pi install /absolute/path/to/agent/pi
```

然后选择 provider **aria**。本地 engine 无需 API key（`auth.resolve` 发送空凭证）。

| 扩展 | 作用 |
|------|------|
| `extensions/aria-engine.ts` | `createProvider` + `openAICompletionsApi`，`fetch` `/v1/models` |
| `extensions/aria-memo.ts` | tools `aria_memo_*`，slash `/memo-search` |

设置 `ARIA_MEMO_AUTO_INJECT=1` 可在 `before_agent_start` 注入检索结果。
