# DeepSeek Harness 插件

[English](README.md) | [中文](README_cn.md)

Out-of-tree Cordis 插件。设置 `ARIA_ENGINE_BUNDLE` + `ARIA_FFI_LIB` 接入 engine，并用 `../scripts/cube-sandbox-up.sh` 拉起 CubeSandbox，再在 dsh checkout 里执行：

```sh
pnpm dsh web --patch /absolute/path/to/agent/dsh/cordis.patch.yml
```

把 [cordis.patch.yml](cordis.patch.yml) 中的 `AGENT_ROOT` 换成该绝对路径。插件路径必须是绝对路径（[dsh 插件教程](https://deepseek-harness.github.io/deepseek-harness/en/develop/basic/)）。

| 插件 | 作用 |
|------|------|
| `plugins/shared` | config / `AriaError` / spawn 助手（仓库内共享，非独立包） |
| `plugins/aria-engine` | `ctx.llm.registerAdapter(['aria'], …)` → engine 进程内 FFI |
| `plugins/aria-memo` | tools `aria_memo_*`；可选 `autoInject`（`agent/pre-step`） |
| `plugins/aria-sandbox` | CubeSandbox（E2B）沙盒 tools `sandbox_*` + `workspace_status`；每 agent 一个隔离的持久化工作空间 |

选择 provider 路由 `aria`。本地 engine 无 API key。沙盒需要先创建 CubeSandbox 模板（`CUBE_TEMPLATE_ID`）——见 `../scripts/cube-sandbox-up.sh`。
