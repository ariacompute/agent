# Architecture

A capability-seam agent runtime, exposed as (a) a Rust + Postgres cloud service and
(b) native SDKs for Swift / Kotlin plus language SDKs for JS / Python. There is no
vendored harness: every capability is a seam with a default provider.

## Layers

```
aria-agent-plugin        manifest + registry (topo sort, cycle/core checks) → frozen
        │
        ▼
aria-agent-core          Agent loop · seams · SessionEvent stream · prompt · token meter
        │                      │
        │                      ├── seams: llm sandbox fs shell jobs storage
        │                      │          sessionPersistence sessionQuery approvals
        │                      │          skills web userQuestions subagents schedule
        ▼                      ▼
providers                aria-agent-tools        (fs / shell / jobs / terminal /
   │                     aria-agent-orchestrator   skills / web / ask / plan / todo / memo)
   │                     aria-agent-sandbox     (docker / kata / cube / browser)
   │                     aria-agent-memo        (aria memo, SQLite)
   ▼
aria-agent-cloud (axum, Postgres + pgvector)  ──►  ariacompute-agent (UniFFI)
                                                     ├── bindings/swift
                                                     └── bindings/kotlin
```

| Crate | Role |
|-------|------|
| `aria-agent-plugin` | `PluginManifest` / `SeamKey` / `SeamRole`, registry builder, dependency topo-sort, cycle + `core`-uniqueness checks, frozen snapshot. |
| `aria-agent-core` | `Agent` runtime: run loop, seam traits, `SessionEvent` stream + projection, prompt assembly, token meter, tool pipeline, permission presets. |
| `aria-agent-tools` | Built-in tool families behind seams. `pty` (terminal) and `web` (HTTP) are **off by default**. |
| `aria-agent-orchestrator` | In-process sub-agents, agent teams (validated task DAG), scheduled jobs (single-flight ticker). |
| `aria-agent-sandbox` | Pluggable `Sandbox`: `DockerSandbox` (default), `KataSandbox`, `CubeSandbox`. |
| `aria-agent-memo` | On-device context store (aria memo / SQLite) implementing `ContextStore`. |
| `aria-agent-browser` | Browser sandbox + browser tool family (reuses the Docker/Kata path). |
| `aria-agent-cloud` | axum service: beta Agents resource, SSE envelope, Postgres + pgvector, teams/jobs routes. |
| `ariacompute-agent` | UniFFI `cdylib` (`libaria-agent_ffi`): `SdkAgent`, `SdkSession`, `create_agent`. |
| `aria-agent-ffigen` | Regenerates the Swift/Kotlin bindings from the cdylib. |

## Storage boundary

* **Cloud**: conversational / long-term context lives in **Postgres + pgvector**
  (`context_fragments`, sharded by `principal_id`). pgvector is required — boot
  fails without it (ADR-0010). Metadata (`agents`, `agent_sessions`,
  `session_turns`, `session_items`, `agent_teams`, `scheduled_jobs`,
  `session_titles`) is `principal_id`-scoped too.
* **Local**: `aria-agent-memo` (SQLite, aria memo) — the only on-device format.
* The shared contract is `aria-agent-core::context`; `aria-agent-core` never
  depends on a store implementation. SDKs pick `cloud` / `local` / `both` per call
  (ADR-0011).

## Run loop

1. `SessionEvent::TurnStarted`; recall fragments (optional graph `expand`).
2. Assemble the system prompt (`base identity → instructions → skills → plan →
   workdir → permissions`, stable order).
3. Loop: token-meter pressure check (compact if needed) → model call (reported
   usage preferred, estimate as fallback) → tool calls through
   `ToolExecutionPipeline` (preset → approval → dispatch → truncate/spill) →
   events.
4. `TurnCompleted` / `TurnFailed`; a context-overflow report compacts **once** and
   retries **once**.
5. Every event goes to the SSE envelope *and* to `sessionPersistence` (JSONL
   locally; in the cloud a title projection into `session_titles`, read back via
   `GET /v1/agents/sessions/{id}/title`).

## Tool pipeline & approvals

`resolve → permission preset (read_only | workspace_write | danger_full_access) →
approval → dispatch → post-process`. Without an approval provider the call is
refused (`unavailable`) and the reason is fed back to the model — never silently
allowed (ADR-0016). `fs`/`shell`/`jobs` are rooted at the single policy directory;
`web_fetch` refuses non-public targets, follows at most five redirects and caps
the body size.

## Sandbox

`aria-agent-sandbox` implements `ExecSpec` / `ExecOutput` with a `SandboxProvider`
abstraction (`docker`, `docker --runtime=kata`, `cube`, `browser`). Docker is the
default; the removed `codex` value is a hard configuration error (ADR-0018).

## FFI & SDKs

`ariacompute-agent` exposes a small stable surface (`SdkAgent`, `SdkSession`,
`SdkAgentConfig`, `SdkStreamEvent`, `create_agent*`). Generate bindings with
`just ffi`; generated Swift/Kotlin sources are committed and never hand-edited
(ADR-0002/ADR-0008). `sdk/js` and `sdk/python` speak the HTTP API and decode the
same frames, including `agent.turn.approval.*` and `agent.turn.usage`.
