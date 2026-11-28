# ADR-0028: the cloud is also an A2A server

## Status

Accepted.

## Context

`aria-agent-cloud` spoke one protocol face: the OpenAI beta Agents API (SSE with
the envelope frozen in `event_envelope.rs`, plus `/v1/agents/**`). An orchestrator
such as agenthub cannot register that as a remote agent — it discovers peers over
the Agent Card convention and drives them over A2A JSON-RPC.

The wire is not ours to design: agenthub has it frozen in
`agenthub/internal/a2a` (types) and `agenthub/internal/adapter/agent.go` (client).
Its actual bytes are the compatibility baseline:

* `params.message` with `parts` carrying **both** `kind` and `type`;
* frames read by `result.kind` = `task | status-update | artifact-update |
  message`, terminal by `final` or a terminal `state`, **no `[DONE]` sentinel**;
* methods `message/send|stream`, `tasks/get|cancel|resume`,
  `tasks/pushNotification/set|get`;
* push body `{taskId, state, config}`, optional `Authorization: Bearer <token>`.

Two local facts shape every decision below (ADR-0027): the default harness is
dsh, and a dsh session **is** a keep-alive container with an attached `exec`
(workspace + `DSH_HOME` on the `aria-dsh-<tenant>-<session>` volume). Tools run
inside that container, so aria's `Sandbox`, `ToolExecutionPipeline` and tool
handlers do not run for such a session, and "stop the harness" means destroying
the container.

## Decision

### 1. A separate projection, not a rewrite

The A2A surface is new code (`aria-agent-cloud/src/a2a/**` on top of the pure
protocol crate `aria-agent-a2a`). `event_envelope.rs`, the `/v1/agents/**` routes
and the SDK are untouched: a beta-Agents client sees no change. A2A reuses the
existing Bearer auth, `Principal`, agent/session stores and scopes — it adds **no
permission code** (`write` for `message/*`, `cancel`, `resume` and push `set`;
`read` for `get`, push `get` and the replay stream).

Cards are public, so their router is merged **after** `require_auth` exactly like
`/healthz`; every JSON-RPC route is registered before it and therefore inherits
Bearer auth, with the scope re-checked inside the dispatcher (a key can be
read-only).

### 2. Errors travel inside HTTP 200

agenthub classifies any non-2xx as `resource_unreachable`, which would hide the
actual code. Therefore:

| Situation | Answer |
| --- | --- |
| envelope is not valid JSON-RPC | `400` + error object |
| method-level failure (incl. `-32602`) | `200` + `{"jsonrpc","id","error"}` |
| missing/invalid key | `401` (existing layer) |
| missing scope | `403` `insufficient_scope` |
| unknown agent card | `404` + `{"error":{"code":"agent_not_found",…}}` |

Codes: `-32700`, `-32600`, `-32601`, `-32602`, `-32603`, `-32001 task_not_found`,
`-32002 task_not_cancelable`, `-32004 agent_not_found`,
`-32005 invalid_state_transition`, `-32006 resource_not_found`,
`-32008 task_not_resumable`.

`message/send` blocks until the task settles (terminal **or** `input-required`)
and then returns the `Task`; business failure is a normal result whose
`status.state` is `failed`. `params.configuration.blocking=false` is accepted and
treated as the default — documented, not silently different.

### 3. `contextId` is a session, and a session is a container

`contextId` maps 1:1 to `agent_sessions.id`, which is what
`DshConfig::for_session` turns into the per-session volume. A request that names an
existing context **visible to the principal** reuses it; anything else creates
`sess_<uuid>` (a client-supplied id is never trusted as a primary key). The
response always carries the server id.

`taskId = task_<uuid>` and `turn_<uuid>` are created together, and the
conversation is mirrored into `session_turns` + `session_items` (user /
function_call / assistant) so the existing console shows it as an ordinary turn.

### 4. One live task per context; cancel destroys the container

`A2aRuntime` holds the live task handles, the context→task reservation and the
session→`PausingHitl` routing table. A second `message/send` in a context that is
still live is refused with `-32005` instead of interleaving two runs.

`tasks/cancel` CASes the state to `canceled` **first** (so a racing completion
cannot overwrite it), then releases a parked decision, aborts the Rust future,
and calls `AppState.cancel_harness` — in `main.rs` a closure over the per-tenant
harness pool registry that runs `DshPool::evict(&session)`. Evicting is what stops
tools inside the container: the pooled runtime is shut down and its sandbox
transport `destroy`s the container. The **volume survives**, so the next prompt in
the same context resumes dsh's history. Cancelling one task never touches another
session's container. On the native harness `cancel_harness` is `None` and the
abort is the whole story.

### 5. HITL is a parked seam, on both harness paths

`PausingHitl` implements `ApprovalBroker` and `UserQuestions`. It fills the
pending slot **before** CASing `working → input-required`, publishes a
`final: true` status frame, and awaits a one-shot channel. `tasks/resume` parses
the answer (`metadata.decision`, then `metadata.answer`, then message text), CASes
`input-required → working`, sends the answer and returns the `working` task. An
answer that does not fit the pending request — text that is not a decision for an
approval, a decision for a free-text question — is `-32602` and **leaves the task
`input-required`**; it is never turned into an implicit denial. `-32008` means
there was nothing parked.

* **native:** the same `Arc<PausingHitl>` is handed to `Agent::with_approvals` and
  `ToolSet::with_questions` through `build_agent_for_a2a`.
* **dsh:** tools run in the container and reach the host as
  `approval/request` / `ask_user_question` on the per-tenant handler.
  `DshPool::acquire` wraps that handler in `SessionTag`, which injects the
  session's `sessionId` into object params that lack one (the dsh JSON-RPC
  protocol itself is unchanged). `RoutingBridge` reads `sessionId`, and if a
  parked A2A task owns that session, delegates to it; otherwise the deployment's
  own handler answers exactly as before (fail-closed).

Parked tasks are not faked into existence: a run enters `input-required` only when
the harness actually asks. While parked, the session keeps its pool slot and its
container, because `resume` must continue in the same runtime.

### 6. The public card carries nothing private, and nothing untrue

`A2A_CARDS_FILE` (optional, camelCase JSON with `description` / `skills`) overrides
the neutral default `"{name} (A2A agent)"` and the single `chat` skill. The card
never contains `instructions`, principals, credentials, tool parameters or
sandbox volume/container names.

It also does **not** advertise aria's native tool families (`shell`, `fs`,
`browser`, …): with the default dsh harness those families are not what an A2A
caller gets — dsh runs its own tools inside the container. A deployment that wants
a precise capability list writes it in `A2A_CARDS_FILE`.

`protocolVersion = "1.0.0"` to match agenthub's `config.ProtocolVersion` (its
client does not validate the field; a consistent display value matters more).
The card is projected per request from the agent row, so a rename is reflected
without a restart.

### 7. The event log is coarse; token deltas are broadcast only

Persisted: the initial `task`, status transitions, tool spans, the final artifact.
Token deltas go to live subscribers only — writing one row per token to Postgres
would cost more than replay is worth. Consequence: `GET /a2a/stream` replays the
coarse log and then follows live, deduplicating a frame that appears in both;
`tasks/get` always has the full text in the artifact.

`GET /a2a/stream` is an HTTP read, not a JSON-RPC call: frames are the stored
`event:`/`data:` pairs with **no envelope**, matching agenthub's
`handleA2AStream`.

### 8. A restart does not pretend

On boot `fail_orphans("process restarted; task cannot continue")` moves every
non-terminal task to `failed` with that message in `status.message`. A subscriber
that reconnects sees a terminal task, not one that looks alive forever.

### 9. Push notifications are best-effort

`http(s)` only, no `user:pass@` in the authority, redirects not followed, 5 s
connect/overall budget, exactly one retry, `token` sent as `Authorization:
Bearer`. The outcome is recorded in `push_last`; a failure is logged and never
rolled back into the task state. The push is awaited **after** the terminal frame
is published, so a slow callback cannot delay a caller's completion.

## Consequences

* A2A tasks consume `DSH_POOL_MAX` slots (default 8 per tenant) for as long as
  they are live, **including while parked**. Pool exhaustion fails the task with a
  retryable message; there is no fallback to the host (ADR-0027).
* Cold start (container spawn + `initialize`, 30 s cap) is part of
  `message/send` latency for a new context.
* `tasks/cancel` loses the in-memory runtime of that session; the volume keeps the
  history. This is the intended meaning of "cancel".
* A normal (non-A2A) OpenAI turn in the same session as a live A2A task could have
  its approval routed to the parked A2A channel: dsh requests carry a session id,
  and the bridge is keyed by session. One-live-task-per-context narrows the
  window, and the A2A side detaches at terminal state; the remaining window is
  accepted and recorded here.
* The card is public, so agent names and `A2A_CARDS_FILE` descriptions are
  visible. A deployment that does not want the name published overrides it there.
* Replay is coarse: a reconnecting client cannot redraw token-by-token output, but
  the final artifact is complete.
* `a2a/pg.rs` is outside the offline line gate for the same reason as
  `store/pg.rs` (needs `DATABASE_URL`). It carries its own DB-gated tests — the
  column round-trip, the CAS transition, the per-task event sequence, tenant
  scoping, push config/receipt and `fail_orphans` — the memory provider pins the
  same contract offline, and the real-database suite applies migration 0010
  through the shared `ensure_schema`.

## Alternatives rejected

* **Deriving card skills from `agent.tools`** — false for the default harness, and
  it publishes tool surface the caller cannot rely on.
* **Falling back to a host process when the sandbox fails** — turns "sandboxed"
  into a claim (ADR-0027).
* **Aborting only the Rust future on cancel** — the container keeps running the
  tool call; cancel must evict.
* **Returning `4xx` for parameter errors** — agenthub reads that as
  `resource_unreachable`.
* **Persisting token deltas** — a row per token for a replay nobody asked for.
* **A "pending HITL" queue per session** — a second simultaneous request would
  need its own task; failing closed is simpler and honest.

## References

* `docs/adr/0027-dsh-sandbox-runtime.md` — a dsh session is a container.
* `docs/adr/0019-dsh-runtime.md` — dsh as the turn runtime.
* `docs/followups/agenthub-a2a.md` — what the orchestrator side must expect.
* `requirements.md` §12 — the wire contract and acceptance criteria.
