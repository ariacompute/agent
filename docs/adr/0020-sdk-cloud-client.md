# ADR-0020: Rust/Swift/Kotlin SDKs are cloud API clients

## Status

Accepted.

## Context

The Rust surface (`ariacompute-agent`, published through UniFFI to Swift and
Kotlin) used to **embed** the whole runtime: `aria-agent-core`, the sandbox, the
tool families, the aria memo store and a provider client were linked into
`libaria-agent_ffi`. In practice that meant:

- an iOS/Android binary carried Postgres-free but still heavy dependencies
  (SQLite, Docker client, HTTP provider, prompt assembly) and a model key had to
  live on the device;
- two implementations of the same semantics (cloud service vs embedded runtime)
  had to be kept in sync by hand;
- `sdk/js` and `sdk/python` already had the better shape: a thin client over the
  beta Agents REST API (`/v1/agents/...`), with memory backends `cloud` /
  `local` / `both` and typed errors.

The user asked for the Rust/Swift/Kotlin SDKs to follow that lead and **remove the
native path**, keeping the JS/Python vocabulary.

## Decision

1. **`ariacompute-agent` is a cloud API client.** It depends only on `reqwest`
   (blocking, rustls), `serde`, `serde_json` and `uniffi`. No `aria-agent-core`,
   no sandbox, no tools, no model client, no embedded context store. Every call
   is an HTTP request to `/v1/agents/...`; SSE is consumed with a line reader.

2. **Vocabulary matches JS/Python** (breaking FFI change, accepted):

   | JS / Python | Rust / Swift / Kotlin |
   | --- | --- |
   | `new Agent(config)` | `AriaAgent::new(AriaAgentConfig)` |
   | `Session.create(agent)` | `agent.createSession()` |
   | `run(agent, input, {session})` | `agent.run(input, session?)` |
   | `runStreamed(...)` | `agent.runStreamed(input, session?, listener, maxTurns?)` |
   | `session.memorize/recall` | `Session.memorize/recall(key, value, backend?)` |
   | `getItems/getTurns/getTitle` | `itemsJson/turnsJson/titleJson` (JSON strings) |
   | `result.finalOutput/history` | `RunResult.finalOutput/historyJson` |
   | `AriaError.kind/status` | `AriaError::kind()/status()` |

   Records use UniFFI's naming (`ClientOptions`, `MemoryConfig`,
   `AriaAgentConfig`, `RunResult`, `TokenUsage`, `StreamEvent`), so Swift/Kotlin
   see camelCase accessors and `runStreamed(input:session:listener:maxTurns:)`.

3. **Transport semantics are the JS ones**: `ARIA_AGENT_BASE_URL` /
   `ARIA_AGENT_API_KEY` as defaults, `OpenAI-Beta: agents=v1`, trailing `/`
   stripped, path segments percent-encoded, `401/403 → auth`, other non-2xx →
   `api { status, message }`, connection failures → `network`, malformed bodies
   → `unknown`, local misuse → `config`.

4. **Memory keeps all three backends.** `cloud` uses
   `GET|POST /v1/agents/sessions/{id}/memory[/{key}]`; `local` spawns the
   `aria-memo` CLI (`--db … add|search --json`, value encoded as `key: value`)
   exactly like `sdk/js`; `both` writes to both and reads merged with the cloud
   copy winning. A missing CLI is an actionable `config` error
   (`install aria-memo` / `ARIA_MEMO_BIN`).

5. **Streaming is synchronous with a callback.** `runStreamed` blocks until the
   terminal frame, invoking `StreamListener.on_event` per frame; it returns a
   `RunResult`. `maxTurns` counts `agent.turn.created` frames and fails with
   `config`; a stream with no frames is an error instead of an empty answer;
   `agent.turn.failed` surfaces as `api`.

6. **Removed**: `create_agent_for_tenant[_with]` (the cloud resolves the tenant
   from the API key), `SdkAgent::session_id` (sessions come from
   `RunResult.session_id`), `sandbox_provider` / `permission_preset` /
   `approval_mode` (server-side configuration), and the whole in-process
   `cloud_context.rs`.

## Consequences

- Mobile binaries get much smaller and need no provider key; they do need
  network access. Offline use is no longer possible for these SDKs.
- `just ffi` regenerates byte-for-byte different bindings; Swift/Kotlin callers
  must migrate (see README examples). The podspec/Package.swift/Gradle
  publishing files are unchanged.
- The FFI crate is testable hermetically: the tests stand a tiny `TcpListener`
  stub in for the service, so `cargo test -p ariacompute-agent` covers the
  request shape, SSE decoding, error mapping and the CLI memory seam without a
  network or a daemon.
- One implementation of the semantics now exists (the cloud service), which also
  removes the old "embedded vs cloud drift" class of bugs.
