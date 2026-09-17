# Follow-up — playground migration to the beta Agents API

`ariacompute/agent` now serves the **OpenAI beta Agents** surface only. The old
routes and the `response.*` envelope are gone, so playground's `/api/agent/*`
calls fail until this migration is done. This document is the hand-off: every
affected location was verified in the playground repo before the switch.

## Endpoint changes

| Old (removed) | New |
| --- | --- |
| `POST /v1/sessions` | `POST /v1/agents/sessions` → `{ id, object: "agent.session", agent, status }` |
| `POST /v1/sessions/{id}/runs` | `POST /v1/agents/sessions/{id}/events` → turn object with `output` |
| `POST /v1/sessions/{id}/runs/stream` | `POST /v1/agents/sessions/{id}/events/stream` (SSE, `agent.*` frames) |
| `GET /v1/agents` | unchanged (still the pool health check) — response is now the full `agent` object |
| `POST /v1/agents` | unchanged path, body may now include `model` / `instructions` / `reasoning` / `tools` |

Sessions are now real rows: **create the session first**, then post events to it.

## Code to change

| File | Current behaviour | Required change |
| --- | --- | --- |
| `backend/internal/agentproxy/proxy.go:37` | `const ReefRecordHeader = "x-reef-agent-record-id"`, forwarded at `:196` | Delete the constant and the header forwarding (header no longer emitted) |
| `backend/internal/agentproxy/proxy.go:63` | `GET {sandbox}/v1/agents` (list) | Keep; parse the new `agent` object (`id`, `name`, `model`) instead of `{id,name}` only |
| `backend/internal/agentproxy/proxy.go:81` | `POST {sandbox}/v1/agents` with body `{name}` | Keep; optionally send `model` / `instructions` |
| `backend/internal/agentproxy/proxy.go:120,143-149` | forwards `POST {sandbox}/v1/sessions/{id}/runs/stream` | Two steps: `POST /v1/agents/sessions` (body `{agent: "agent-demo"}`) then `POST /v1/agents/sessions/{sid}/events/stream` |
| `backend/cmd/server/main.go:87-89`, `backend/internal/pool/pool.go:116` | `HealthPath: "/v1/agents"` | No change needed |
| `frontend/src/lib/api.js:135-207` | parses `response.*` frames; `:220` dispatches on `type.startsWith('response.')` | Parse `agent.*`: dispatch on `type.startsWith('agent.')`; text = `agent.turn.output_text.delta.delta`; tool calls = `agent.turn.item.added` / `agent.turn.item.done` (`item.type === 'function_call'`, output in `item.result.content`) |
| `frontend/src/lib/api.js:187-191` | terminal = `response.completed`, reads `metadata.reef_record_id` | terminal = `agent.turn.completed` (or `agent.turn.failed`); drop the receipt id entirely |
| `frontend/tests/api.spec.js:119-155` | asserts `response.created` / `response.completed` + reef metadata | Rewrite against `agent.turn.created` / `agent.turn.completed` |
| `frontend/src/lib/playground-copy.js` | `agentMetaReceipt: 'Reef receipt'` (13 languages) | Remove/retire the key |
| `sandbox/Dockerfile.agent`, `sandbox/start-agent.sh:131-151` | `REEF_DIR`, `MEMO_DIR`, `AGENT_MEMO_BACKEND` | Drop all three; Postgres image → `pgvector/pgvector:pg16` |
| `backend/internal/config/config.go:215-223` | `AGENT_MEMO_BACKEND` | Remove the setting |

## SSE frame reference (new)

```
agent.turn.created        { turn_id, session_id, turn{status} }
agent.turn.in_progress    { phase, label }            # recall / model / tool_exec / loop_guard
agent.turn.item.added     { item{id,type,status,...} }
agent.turn.item.done      { item{..., result:{content,is_error}} }
agent.turn.output_text.delta { item_id, delta }
agent.turn.output_text.done  { item_id, text }
agent.turn.completed      { turn{status:"completed", output} }   # terminal
agent.turn.failed         { turn{status:"failed"}, error }       # terminal
```

Every frame carries `event_id`, `session_id`, `turn_id`, `sequence_number`.
There is **no** `data: [DONE]` sentinel.

## Acceptance

1. `go test -race ./...` green (proxy tests updated).
2. `bun run test` green (api.spec.js rewritten).
3. Manual: `/agent` playground lists agents, creates a session, streams a reply,
   shows tool calls, and stops on `agent.turn.completed`.
4. No reference to `reef` remains in backend or frontend.
