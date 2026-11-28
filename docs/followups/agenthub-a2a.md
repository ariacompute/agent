# Follow-up — wiring agentbuddy/agenthub to this A2A server

`aria-agent-cloud` now serves A2A (`docs/adr/0028-a2a-server.md`). This note is
what the **orchestrator side** needs to know; nothing in agenthub was changed
from this repository.

## Discovery

| Endpoint | Auth | Card |
| --- | --- | --- |
| `GET /.well-known/agent-card.json` | none | the `A2A_DEFAULT_AGENT` agent, `url = {A2A_PUBLIC_URL}/a2a` |
| `GET /.well-known/agent.json`, `GET /a2a/agent-card` | none | aliases of the above |
| `GET /v1/agents/{id}/.well-known/agent-card.json`, `.../agent-card.json` | none | that agent, `url = {A2A_PUBLIC_URL}/v1/agents/{id}/a2a` |

`A2A_PUBLIC_URL` is the origin the **caller** reaches the cloud on (the card
cannot infer it from a proxied request). Without it the card advertises
`http://127.0.0.1:<port>` and boot logs a warning — fine for a same-host smoke
test, wrong for a real registration.

`protocolVersion` is `1.0.0`. `skills` is a single neutral `chat` entry unless the
deployment supplies `A2A_CARDS_FILE`; the card deliberately does not advertise the
native tool families, because the default dsh harness runs its own tools inside a
sandbox container.

## Calls

`POST {card.url}` with `Authorization: Bearer <key>`, `content-type:
application/json`, JSON-RPC 2.0. A key needs the `write` scope for `message/*`,
`tasks/cancel`, `tasks/resume`, `tasks/pushNotification/set`, and `read` for
`tasks/get`, `tasks/pushNotification/get`, `GET /a2a/stream`.

* `message/send` → `result` is a `Task`. It returns when the task settles: a
  terminal state **or** `input-required`. `params.configuration.blocking=false`
  is accepted and ignored (it behaves as the default).
* `message/stream` → SSE. Each frame is
  `data: {"jsonrpc":"2.0","id":<request id>,"result":<frame>}` with
  `event: task|task_status_update|task_artifact_update|message`. The terminal or
  parked frame has `final: true`; **there is no `[DONE]` sentinel** — the stream
  just ends.
* `tasks/get` → `id` (or `taskId`), optional `historyLength` (0 = no messages).
* `tasks/cancel` → terminal `canceled`, and the session's container is destroyed.
* `tasks/resume` → `id` plus `metadata.decision` (`allow` / `deny`) for an
  approval, `metadata.answer` or message text for a question. An answer that does
  not fit the parked request is `-32602` and the task **stays** `input-required`.
* `tasks/pushNotification/set|get` → `taskId` (or `id`) plus
  `pushNotificationConfig` (or the v0.1 `pushNotification`). `get` with no config
  is `-32006`.
* v0.1 aliases `tasks/send` and `tasks/sendSubscribe` are accepted.

## Replay

`GET /a2a/stream?task=<id>&since=<seq>` (Bearer, `read`) replays the persisted
coarse log and then follows the live task. Unlike `message/stream`, these frames
are **not** wrapped in a JSON-RPC envelope — `event:`/`data:` where `data` is the
frame itself, which is what `handleA2AStream` already reads. `since` past the end
is an empty 200. Token deltas are never persisted, so a reconnect gets status,
tool spans and the final artifact, not the token-by-token text; `tasks/get` has
the full answer.

## Errors

Method-level failures come back as **HTTP 200** with a JSON-RPC `error` object —
treating a non-2xx response as `resource_unreachable` would hide the real code.
Only a malformed envelope is `400`, auth is `401`, a missing scope is `403`
`insufficient_scope`, and an unknown card is `404` with
`{"error":{"code":"agent_not_found",…}}`.

Local codes worth branching on: `-32001 task_not_found`, `-32002
task_not_cancelable`, `-32004 agent_not_found`, `-32005
invalid_state_transition` (another task still owns the context), `-32006
resource_not_found`, `-32008 task_not_resumable`.

## Push callbacks

Body: `{"taskId","contextId","state","status","config"}`, sent for
`input-required`, `completed`, `failed` and `canceled`. `config.token` arrives as
`Authorization: Bearer <token>`. Targets must be `http(s)` without userinfo,
redirects are not followed, budget is 5 s with one retry, and a delivery failure
never changes the task state (it is recorded in `push_last`).

## Operational notes

* A live A2A task occupies one dsh pool slot (`DSH_POOL_MAX`, default 8 per
  tenant) **while parked too**, because `resume` must continue in the same
  container. Pool exhaustion fails the task with a retryable message; the request
  does not fall back to a host process.
* A new context pays container spawn + `initialize` (30 s cap) before the first
  token.
* `tasks/cancel` destroys that context's container. The session volume survives,
  so a later prompt in the same `contextId` resumes dsh's history.
* A process restart fails every non-terminal task (`status.message` =
  "process restarted; task cannot continue"). Reconnect and read `tasks/get`
  rather than assuming the stream is still alive.
