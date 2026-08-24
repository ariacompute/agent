# DeepSeek Harness plugins

[English](README.md) | [中文](README_cn.md)

Out-of-tree Cordis plugins. Set `ARIA_MODEL_BUNDLE` + `ARIA_FFI_LIB` for the model bundle,
bring up CubeSandbox (`../scripts/cube-sandbox-up.sh`), then from a dsh checkout:

```sh
bun dsh web --patch /absolute/path/to/agent/dsh/cordis.patch.yml
```

Replace `AGENT_ROOT` in [cordis.patch.yml](cordis.patch.yml) with that absolute path.
Plugin paths must be absolute ([dsh plugin tutorial](https://deepseek-harness.github.io/deepseek-harness/en/develop/basic/)).

| Plugin | Role |
|--------|------|
| `plugins/shared` | config / `AriaError` / spawn helpers (in-repo, not a package) |
| `plugins/aria-engine` | `ctx.llm.registerAdapter(['aria'], …)` → engine in-process FFI |
| `plugins/aria-memo` | tools `aria_memo_*`; optional `autoInject` on `agent/pre-step` |
| `plugins/aria-sandbox` | CubeSandbox (E2B) sandbox tools `sandbox_*` + `workspace_status`; isolated persistent per-agent workspaces |

Select provider route `aria`. Local engine has no API key. Sandbox needs a CubeSandbox
template (`CUBE_TEMPLATE_ID`) — see `../scripts/cube-sandbox-up.sh`.
