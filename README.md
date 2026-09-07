# aria agent

[English](README.md) | [中文](README_cn.md)

Out-of-tree integration of Aria components into **DeepSeek Harness (`dsh`)**.

- **LLM backend**: `engine` in-process via the `aria-engine` plugin (`@ariacompute/engine-ts` FFI SDK).
- **Context memory**: `memo` via the `aria-memo` plugin (tools `aria_memo_*`, optional `autoInject`).
- **Agent sandbox**: [CubeSandbox](https://github.com/TencentCloud/CubeSandbox) (E2B-compatible)
  via the `aria-sandbox` plugin — each agent gets an isolated, persistent workspace.
- **Self-improvement**: `aria-reef` — a Reef-style loop that records every turn, binds feedback to it,
  evolves skills / prompts / rules (local engine FFI) or dispatches weight training (ariapin),
  keeps the winner, versions it in Git and hot-serves it back. **Off by default.**

## Architecture

```
dsh/plugins/
├── shared/          config / AriaError / spawn helpers (replaced aria-bridge)
├── aria-engine/     ctx.llm.registerAdapter(['aria'], …) → engine in-process FFI (tool calls)
├── aria-memo/       aria_memo_add/search/get/list/forget + autoInject (off by default)
├── aria-sandbox/    sandbox_exec / sandbox_read_file / sandbox_write_file /
│                    sandbox_list_files / sandbox_sync_to_host / sandbox_sync_from_host /
│                    workspace_status  (CubeSandbox via e2b SDK)
└── aria-reef/       Serve (record_id) → Observe (report/outcome/rubric) →
                     Grow (skillclaw / prompt / rules / weight) →
                     Commit (git + LFS) → Surface (hot reload)
scripts/cube-sandbox-up.sh   one-shot local CubeSandbox bootstrap + template
```

## Requirements

- [Bun](https://bun.com) >= 1.4 (`bun install` / `bun test` / `bunx tsc`).
- `aria-engine` bundle at `ARIA_MODEL_BUNDLE` and native lib at `ARIA_FFI_LIB` (no HTTP server needed).
- `aria-memo` CLI reachable at `ARIA_MEMO_BIN` (default `aria-memo`).
- CubeSandbox: x86_64 Linux with KVM (`/dev/kvm`); see [CubeSandbox](https://github.com/TencentCloud/CubeSandbox).

## Setup

```sh
bun install             # use https_proxy=http://127.0.0.1:7897 if the network requires it
scripts/cube-sandbox-up.sh   # checks KVM, installs CubeSandbox, creates the template, writes .env
```

## Run

```sh
export ARIA_MODEL_BUNDLE=/path/to/aria/model/bundle
export ARIA_FFI_LIB=/usr/lib/libaria_ffi.so
bun dsh web --patch /absolute/path/to/agent/dsh/cordis.patch.yml
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

See `requirements.md` §2 for the full env table (`ARIA_MODEL_BUNDLE`, `ARIA_FFI_LIB`,
`ARIA_ENGINE_MODEL`, `ARIA_MEMO_*`,
`E2B_API_URL`, `E2B_API_KEY`, `CUBE_TEMPLATE_ID`, `E2B_TIMEOUT_MS`,
`ARIA_WORKSPACE_ROOT`, `ARIA_WORKSPACE_SYNC_AFTER_EXEC`, `ARIA_WORKSPACE_ID`,
`ARIA_REEF_*`).

## Self-improvement (aria-reef)

```sh
export ARIA_REEF_ENABLED=on
export ARIA_REEF_RELEASE=v1            # stamped on every recorded turn
export ARIA_REEF_BUNDLE=$ARIA_MODEL_BUNDLE   # harness recipes propose edits with the local engine
export ARIA_REEF_AUTO_APPLY=on         # publish accepted candidates automatically
# weight recipe (optional): export ARIA_REEF_RECIPES=skillclaw,prompt,rules,weight
#                           export ARIA_REEF_ARIAPIN_URL=http://127.0.0.1:8001
#                           export ARIA_REEF_ARIAPIN_KEY=... ARIA_REEF_BASE_MODEL=...
# automatic cycles (off by default):
#                           export ARIA_REEF_CYCLE=on ARIA_REEF_CYCLE_INTERVAL_MS=900000
#                           export ARIA_REEF_CYCLE_MIN_SIGNALS=8
```

- Every turn gets a `record_id` (surfaced as `reef.recordId`); recording is fire-and-forget and
  never blocks the agent loop.
- Feedback: `aria_reef_report` (score 0..1 or 1..5 / text / structured JSON), task-outcome signals,
  and a deterministic rubric scorer.
- `aria_reef_cycle` runs Grow → Evaluate → Commit → Surface on demand; `aria_reef_status` shows counters.
- Optional automatic cycles (`ARIA_REEF_CYCLE=on`): a scheduler runs a cycle when the interval
  elapsed **or** enough new signals accumulated — single-flight, detached from the agent loop,
  with exponential backoff on failures. `aria_reef_status` then also reports `autoCycle`.
- Accepted harness artifacts are read from disk every turn (hot reload, no restart); weights are
  reloaded through ariapin (`POST /v1/models/{id}/reload`, falling back to an agent restart).
- Storage: `$ARIA_REEF_STORE_DIR` (JSONL) + `$ARIA_REEF_ARTIFACT_REPO` (local git, weights via LFS).

## Development

```sh
bun test            # offline unit tests (shared + engine + memo + sandbox + reef)
bun run typecheck   # bunx tsc --noEmit across plugins
```

Notes: memo `search` returns `score\tcontent` lines (no ids). `engine` must be
serving before use. `model/` is out of scope. GitHub/registry access may need
`export https_proxy=http://127.0.0.1:7897`.
