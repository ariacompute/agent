# agent

[English](README.md) | [中文](README_cn.md)

Out-of-tree plugins that connect [engine](https://github.com/ariacompute/engine) and [memo](https://github.com/ariacompute/memo) to [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) and [Pi](https://github.com/earendil-works/pi). 

- **engine** — LLM adapter / custom provider against `aria-engine serve` (OpenAI HTTP).
- **memo** — memory tools plus optional context injection via the `aria-memo` CLI.

## Prerequisites

1. Start the engine:

```sh
aria-engine serve <bundle_or_model> --bind 127.0.0.1:8080
```

2. Build memo if it is not already on `PATH`:

```sh
cargo build -p aria-memo --release
export ARIA_MEMO_BIN=/path/to/aria-memo
```

## Environment

| Variable | Default |
|----------|---------|
| `ARIA_ENGINE_URL` | `http://127.0.0.1:8080/v1` |
| `ARIA_MEMO_BIN` | `aria-memo` |
| `ARIA_MEMO_DB` | `~/.ariacompute/memo.db` |

## DeepSeek Harness

From a dsh checkout that can run `pnpm dsh`:

```sh
pnpm dsh web --patch /absolute/path/to/agent/dsh/cordis.patch.yml
```

Edit [dsh/cordis.patch.yml](dsh/cordis.patch.yml) so plugin `name` paths are absolute on your machine. Details: [dsh/README.md](dsh/README.md).

## Pi

```sh
pi install /absolute/path/to/agent/pi
```

Select provider `aria` after the engine is up. Details: [pi/README.md](pi/README.md).

## Develop

```sh
npm install
npm test
npm run typecheck
```
