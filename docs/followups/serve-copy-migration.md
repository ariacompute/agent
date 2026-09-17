# Follow-up — serve marketing copy migration

The `serve` repo has no code that calls the agent API; only its marketing copy
describes the old endpoints and the removed Reef feature. Nothing is broken at
runtime — but the published copy is now inaccurate.

> Note: at the time of writing the serve worktree has **staged, uncommitted**
> changes to `frontend/src/lib/agent-copy.js`,
> `frontend/src/lib/sdk-bindings-copy.js` and `requirements.md`. Apply the
> changes below on top of those.

## `frontend/src/lib/agent-copy.js` (13 languages share the same keys)

| Location | Current | Required change |
| --- | --- | --- |
| `:47`, `:55` | `POST /v1/agents`, `POST /v1/sessions/s1/runs`, `POST /v1/sessions/s1/runs/stream` | `POST /v1/agents`, `POST /v1/agents/sessions`, `POST /v1/agents/sessions/{sid}/events/stream` |
| `:64-67` | Reef capability copy + `/reef/report`, `/reef/evolve` | Delete the Reef band and endpoints (feature removed) |
| `:33`, `:96` | `Bearer` / `ApiKey` auth copy | Add the `OpenAI-Beta: agents=v1` header to the examples |
| `:61` | `sdkSub`: "Python, Rust, Swift, Kotlin and TypeScript native bindings" | Mention the new packages: `npm i @ariacompute/agent`, `pip install ariacompute-agent` |

## `frontend/src/lib/sdk-bindings-copy.js`

Currently documents only engine / router packages (`aria-engine`,
`@ariacompute/engine-ts`, `ariarouter-ts`). Add the agent SDKs:

```bash
npm install @ariacompute/agent
pip install ariacompute-agent
```

with a quickstart matching the Agents SDK shape (`Agent` + `run` / `Runner.run`).

## Pre-render / SEO

`frontend/src/views/AgentView.vue` pre-renders the Agent page, and the Reef
band is part of that output. After editing the copy re-run:

```bash
cd frontend && bun run build && bun run validate:seo
```

## Acceptance

1. No `/v1/sessions` or `/reef/` endpoint remains in any language file.
2. Endpoint examples match the new beta Agents surface.
3. `bun run build` + `bun run validate:seo` pass.
