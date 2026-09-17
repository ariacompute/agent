# Follow-up — cockpit client migration

The cockpit Android app talks to the cloud over SSE and to the native SDK
in-process. The cloud half must migrate to the beta Agents API; the on-device
half keeps working (the embedded store was preserved) but should be re-verified
against the regenerated bindings.

## `app/.../data/CloudClient.kt`

| Location | Current | Required change |
| --- | --- | --- |
| `:35-38` | `POST $baseUrl/v1/sessions/default/runs/stream`, `Authorization: Bearer`, `Accept: text/event-stream` | Two-step: `POST /v1/agents/sessions` (body `{"agent":"cockpit"}`) → `POST /v1/agents/sessions/{sid}/events/stream` |
| `:28-32` | body `{ "agent": "cockpit", "input": ..., "stream": true }` | body `{ "input": ... }` (agent is bound to the session) |
| `:49-66` | parses `response.output_text.delta`, falls back to a top-level `token` field | parse `agent.turn.output_text.delta` (`delta`); terminal events are `agent.turn.completed` / `agent.turn.failed`; **delete** the legacy `token` fallback |

Frame notes: every frame carries `event_id`, `session_id`, `turn_id`,
`sequence_number`; there is no `data: [DONE]` sentinel.

## `app/.../data/MemoStore.kt`

* Uses `com.ariacompute.agent.uniffi.aria_agent_ffi.*` with
  `createAgent(...).session()` → `memorize(key, value)` / `recall(key)`.
* The on-device store is unchanged (sled), but `SdkAgentConfig` gained an
  `instructions` field and the bindings were regenerated — re-verify the
  constructor call and the method names after upgrading the AAR.

## Optional

| Location | Change |
| --- | --- |
| `app/.../MainActivity.kt:23-24` | `CloudClient("http://10.0.2.2:3000", "")` is hardcoded — make it configurable |
| `app/build.gradle.kts:54` | `com.ariacompute:agent:0.1.0` — bump after the next release |

## Architecture note

`cockpit/AGENTS.md` rule 1 still says "agent session context lives only in the
on-device memo (sled)". That remains true for the **on-device** path; the cloud
side now stores context in Postgres + pgvector. Update the cockpit docs so the
two paths are described separately.

## Acceptance

1. Live reply streams again (verified on device/emulator against the new
   endpoint).
2. No `response.*` parsing or `token` fallback remains.
3. Memo `memorize` / `recall` still work with the regenerated bindings.
