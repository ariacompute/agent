# ADR 0008: SDK interface ergonomics (Agents SDK shape)

## Status

Accepted (revised 2026-09-17 to add JS/Python SDKs and the beta Agents API).

## Context

The native (Swift/Kotlin) SDK and cloud clients exposed the session/thread id to
the caller. That is more verbose than the upstream ergonomics we want to match:

* The **OpenAI Agents SDK** quickstart runs `run(agent, input)` (JS) /
  `Runner.run(agent, input)` (Python) with no explicit session in the happy path,
  and returns `result.finalOutput` / `result.final_output`.
* The **codex** agents SDK follows the same shape: create an agent, call `run`
  with input, get a result.

## Decision

1. **Session is an internal context-scope key, not a public constructor
   argument.** The native `create_agent(name, model)` owns an opaque session
   exposed via `agent.session()` / `agent.session_id()`; the happy path mirrors
   `Runner.run(agent, input)`.
2. **Official JS and Python SDKs** ship in `sdk/js` (`@ariacompute/agent`) and
   `sdk/python` (`ariacompute-agent`). Their public shape mirrors
   `@openai/agents` / `openai-agents`:
   * `new Agent({ name, instructions, model, tools, handoffs })` /
     `Agent(name=, instructions=, model=, tools=[...])`
   * `run(agent, input)` / `await Runner.run(agent, input)`
   * `runStreamed(agent, input)` / `await Runner.run_streamed(agent, input)`
     (async iterable of `agent.*` frames + `completed` promise)
   * `tool({...})` / `function_tool`
   * `Session.create(agent)` for multi-turn continuity
   * results expose `finalOutput` / `final_output`, `history` (and
     `to_input_list()` in Python)
3. **These SDKs are thin clients over the beta Agents REST API** (ADR-0009):
   create/resolve a session, then `POST …/events` or `…/events/stream`. They do
   not re-implement the agent loop; tool *execution* stays in the cloud sandbox,
   so `execute` is local-only and never forwarded.
4. **Native SDK keeps the embedded store.** Swift/Kotlin continue to run
   in-process with `aria-agent-memo` (sled) and offline-first behaviour; their
   surface stays session-less.

## Consequences

* One ergonomic model across languages; the OpenAI quickstart transfers verbatim.
* `just ffi` must be re-run (and bindings committed) whenever the UniFFI surface
  changes — the `instructions` field was added to `SdkAgentConfig`.
* SDK tests run without a server: JS via `bun test`, Python via `unittest`.
