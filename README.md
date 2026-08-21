# aria agent

[English](README.md) | [中文](README_cn.md)

Out-of-tree integration of Aria components into **DeepSeek Harness (`dsh`)**.

- **LLM backend**: `engine` as an OpenAI-compatible server via the `aria-engine` plugin.
- **Context memory**: `memo` via the `aria-memo` plugin (tools `aria_memo_*`, optional `autoInject`).
- **Agent sandbox**: [CubeSandbox](https://github.com/TencentCloud/CubeSandbox) (E2B-compatible)
  via the `aria-sandbox` plugin — each agent gets an isolated, persistent workspace.

## Architecture

```
dsh/plugins/
├── shared/          config / AriaError / spawn helpers (replaced aria-bridge)
├── aria-engine/     ctx.llm.registerAdapter(['aria'], …) → engine OpenAI SSE (tool calls)
├── aria-memo/       aria_memo_add/search/get/list/forget + autoInject (off by default)
└── aria-sandbox/    sandbox_exec / sandbox_read_file / sandbox_write_file /
                     sandbox_list_files / sandbox_sync_to_host / sandbox_sync_from_host /
                     workspace_status  (CubeSandbox via e2b SDK)
scripts/cube-sandbox-up.sh   one-shot local CubeSandbox bootstrap + template
```

## Requirements

- Node >= 18.18, pnpm.
- `aria-engine` binary reachable at `ARIA_ENGINE_URL` (default `http://127.0.0.1:8080/v1`).
- `aria-memo` CLI reachable at `ARIA_MEMO_BIN` (default `aria-memo`).
- CubeSandbox: x86_64 Linux with KVM (`/dev/kvm`); see [CubeSandbox](https://github.com/TencentCloud/CubeSandbox).

## Setup

```sh
pnpm install            # use https_proxy=http://127.0.0.1:7897 if the network requires it
scripts/cube-sandbox-up.sh   # checks KVM, installs CubeSandbox, creates the template, writes .env
```

## Run

```sh
aria-engine serve <bundle> --bind 127.0.0.1:8080 &
pnpm dsh web --patch /absolute/path/to/agent/dsh/cordis.patch.yml
```

Select provider route `aria`. The sandbox template id is read from `CUBE_TEMPLATE_ID`
(env or `.env`); without it `sandbox_*` tools will fail loudly.

## Workspaces (isolation)

- Each agent session maps to one workspace id: tool arg `workspace` → `sessionId` → config → `default`.
- Host dir: `$ARIA_WORKSPACE_ROOT/<workspaceId>/` (default `~/.ariacompute/agent/workspaces`, mode 0700).
- Registered with dsh `ctx.workspaceRegistry` when available (falls back to plain dirs).
- Isolation: one KVM MicroVM per workspace (CubeSandbox) + separate 0700 host dirs +
  tool-level path checks (`/workspace` only, no `..` escapes).
- Persistence: `sandbox_sync_to_host` / `sandbox_sync_from_host`; optional
  `ARIA_WORKSPACE_SYNC_AFTER_EXEC=true` to auto-pull after every `sandbox_exec`.

## Configuration

See `requirements.md` §2 for the full env table (`ARIA_ENGINE_URL`, `ARIA_MEMO_*`,
`E2B_API_URL`, `E2B_API_KEY`, `CUBE_TEMPLATE_ID`, `E2B_TIMEOUT_MS`,
`ARIA_WORKSPACE_ROOT`, `ARIA_WORKSPACE_SYNC_AFTER_EXEC`, `ARIA_WORKSPACE_ID`).

## Development

```sh
npm test            # offline unit tests (shared + engine + memo + sandbox)
npm run typecheck
```

Notes: memo `search` returns `score\tcontent` lines (no ids). `engine` must be
serving before use. `model/` is out of scope. GitHub/registry access may need
`export https_proxy=http://127.0.0.1:7897`.
