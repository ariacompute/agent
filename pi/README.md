# Pi package

[English](README.md) | [中文](README_cn.md)

Install from a trusted local path ([Pi packages](https://pi.dev/docs/latest/packages)):

```sh
aria-engine serve <bundle> --bind 127.0.0.1:8080
pi install /absolute/path/to/agent/pi
```

Then pick provider **aria**. Local engine needs no API key (`auth.resolve` sends empty credentials).

| Extension | Role |
|-----------|------|
| `extensions/aria-engine.ts` | `createProvider` + `openAICompletionsApi`, `fetch` `/v1/models` |
| `extensions/aria-memo.ts` | tools `aria_memo_*`, slash `/memo-search` |

Set `ARIA_MEMO_AUTO_INJECT=1` to inject search hits on `before_agent_start`.
