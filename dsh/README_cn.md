# DeepSeek Harness 插件

[English](README.md) | [中文](README_cn.md)

Out-of-tree Cordis 插件。先启动 `aria-engine serve`，再在 dsh checkout 里执行：

```sh
pnpm dsh web --patch /absolute/path/to/agent/dsh/cordis.patch.yml
```

把 [cordis.patch.yml](cordis.patch.yml) 中的 `AGENT_ROOT` 换成该绝对路径。插件路径必须是绝对路径（[dsh 插件教程](https://deepseek-harness.github.io/deepseek-harness/en/develop/basic/)）。

| 插件 | 作用 |
|------|------|
| `plugins/aria-engine` | `ctx.llm.registerAdapter(['aria'], …)` → OpenAI HTTP |
| `plugins/aria-memo` | tools `aria_memo_*`；可选 `autoInject`（`agent/pre-step`） |

选择 provider 路由 `aria`。本地 engine 无 API key。
