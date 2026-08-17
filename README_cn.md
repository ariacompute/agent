# agent

[English](README.md) | [中文](README_cn.md)

将 [engine](https://github.com/ariacompute/engine) 与 [memo](https://github.com/ariacompute/memo) 以 out-of-tree 插件接到 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 和 [Pi](https://github.com/earendil-works/pi)。

- **engine** — 作为 LLM adapter / custom provider，打 `aria-engine serve` 的 OpenAI HTTP。
- **memo** — 记忆 tools + 可选上下文注入（`aria-memo` CLI）。

## 前置

1. 启动 engine：

```sh
aria-engine serve <bundle_or_model> --bind 127.0.0.1:8080
```

2. memo 不在 `PATH` 时先编译：

```sh
cargo build -p aria-memo --release
export ARIA_MEMO_BIN=/path/to/aria-memo
```

## 环境变量

| 变量 | 默认 |
|------|------|
| `ARIA_ENGINE_URL` | `http://127.0.0.1:8080/v1` |
| `ARIA_MEMO_BIN` | `aria-memo` |
| `ARIA_MEMO_DB` | `~/.ariacompute/memo.db` |

## DeepSeek Harness

在可运行 `pnpm dsh` 的 checkout 上：

```sh
pnpm dsh web --patch /absolute/path/to/agent/dsh/cordis.patch.yml
```

把 [dsh/cordis.patch.yml](dsh/cordis.patch.yml) 里的插件 `name` 换成你机器上的绝对路径。详见 [dsh/README_cn.md](dsh/README_cn.md)。

## Pi

```sh
pi install /absolute/path/to/agent/pi
```

engine 起来后选择 provider `aria`。详见 [pi/README_cn.md](pi/README_cn.md)。

## 开发

```sh
npm install
npm test
npm run typecheck
```
