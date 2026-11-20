# ADR-0019: DeepSeek Harness as the turn runtime

## Status

Accepted.

## Context

This workspace is a capability-seam agent runtime whose seams were **modelled on**
DeepSeek Harness (dsh, `github.com/deepseek-ai/deepseek-harness`): the plugin
registry, the four role kinds, the tool-execution pipeline and the session event
stream all mirror dsh's contracts, but the turn loop itself was a private Rust
implementation. There was, explicitly, "no vendored harness".

Users now want the opposite: dsh should be the **default runtime**, so that a turn
(turn/step orchestration, model call, tool execution, sandbox, approvals) is
executed by dsh, while this workspace keeps owning the API surface, memory,
tenancy and the streaming contract.

Constraints that made this non-trivial:

- the streaming envelope (`aria-agent-cloud::event_envelope`) is **frozen**
  (rule 9): we cannot let dsh's frame names reach the wire;
- there is no approval provider on a headless deployment, and rule 4 requires
  "no provider means denied";
- every table/session is `principal_id`-scoped (rule 7), and dsh keeps mutable
  state in `DSH_HOME`, so homes must never be shared;
- dsh is in developer preview: its `SessionEvent` payloads are not a frozen
  schema, and its HTTP "api-gateway" docs describe an **internal** Typert gateway
  (`POST /api//` + `/api/remote.mux` WebSocket), not a public REST API — the
  generally usable HTTP surface lives in third-party plugins
  (`dsh-api-gateway`, `dsh-openai-gateway`, `dsh-llm-gateway`).

## Decision

### 1. dsh is an external child process, not vendored code

`dsh --profile sdk` is spawned and driven over **newline-delimited JSON-RPC 2.0
on stdio** (the protocol the official Python SDK uses; the SDK ships the runtime
binary in `deepseek-harness-runtime-bin`, so no system Node.js is involved).
We deliberately do **not** depend on third-party gateway plugins: the default
runtime must not be built on non-official middleware, and stdio needs no port,
no auth plane and no plugin install.

New crate: **`crates/aria-agent-dsh`** (`core` + `plugin` + tokio only).

### 2. The turn loop becomes a real seam

`AGENT_LOOP` was a declared bundle key with no consumer. It is now a **seam**:

```rust
pub struct LoopContext { /* one per turn: model, context, sandbox, pipeline,
                            prompt, session log, tools, title handles, config */ }

#[async_trait]
pub trait AgentLoop: Send + Sync {
    async fn run(&self, ctx: LoopContext, input: &str)
        -> Result<BoxStream<'static, Result<AgentEvent, CoreError>>, CoreError>;
    async fn run_once(&self, ctx: LoopContext, input: &str) -> Result<String, CoreError> { … }
}
```

- `NativeLoop` is the old loop body moved verbatim behind the contract
  (`Agent::run_event_stream` now only builds the context and delegates).
- `DshAgentLoop` implements it by prompting the runtime and mapping notifications.
- `AgentConfig::harness` selects the runtime: `native` (library default) or
  `dsh`. `default_agent_loop` **errors** on `dsh` (the harness must be injected
  through `Agent::with_components`) and on any unknown value — no silent
  fallback. `Agent::with_handlers` keeps its signature and applies the same rule.
- Seams are still resolved once, at assembly (ADR-0014); `LoopContext` only
  clones handles.

### 3. Event mapping is one pure function

`aria-agent-dsh::event_map::map_notification` translates dsh frames into a
wire-neutral `Mapped` enum; `DshAgentLoop` converts that to `AgentEvent` +
`SessionEvent`. The cloud keeps translating `AgentEvent` → `agent.*` frames, so
**`event_envelope.rs` is untouched**.

| dsh notification | agent event | session record |
| --- | --- | --- |
| `turn/start` | `Step{phase:"harness"}` | `step` |
| `step/start` | `Step{phase:"step"}` | `step` |
| `tool/call` | `Step{phase:"tool_exec", label:name}` | — |
| `tool/result` | `ToolCall{name, arguments, result}` | `item_added` + `item_done` |
| `assistant/message` (usage) | `Usage{…}` | `usage` |
| `assistant/message` (text) | `Token{text}` | `text_done` |
| `turn/end` | `Done{text}` (or `Err` when `reason.kind == "error"`) | `turn_completed` / `turn_failed` |
| `error`, `agent/error` | `Err` | `turn_failed` |
| `turn/end` without `reason.kind` | `Err` (protocol) | `turn_failed` |
| unknown / transient | ignored | — |

Ordering invariants are preserved (`Usage` before `Token` before `Done`), the
stream is bounded (64-slot channel), and a runtime that closes without
`turn/end` fails the turn instead of yielding an empty answer.

### 4. Approvals and questions go through the seams

`approval/request` → `ApprovalBroker::decide` → `{"decision":"allow"|"deny"}`;
`ask_user_question` → `UserQuestions::ask`. A missing provider answers **deny**
with the reason (or a JSON-RPC error for questions), so a harness turn is exactly
as strict as a native one. Approval decisions are emitted from the harness side;
they are not re-emitted as `AgentEvent::Approval` (that event is a native-pipeline
detail).

### 5. Tenancy: one `DSH_HOME` and workspace per session

`DshConfig::for_session(root, session, model)` derives
`<root>/.aria/dsh/<session>` (home) and `<root>/.aria/dsh-workspaces/<session>`
(cwd), with the id sanitised to a filesystem-safe leaf. `~/.dsh` is never
discovered implicitly — dsh requires an explicit `DSH_HOME` and so do we. The
child environment is an **allow-list** (`DSH_HOME`, `PATH`, the `GATEWAY_*`
values bridged into the two child env names dsh reads, explicit extras), never
the whole process environment. There are no separate `DEEPSEEK_*` or `OPENAI_*`
deployment variables, and no `GATEWAY_* → OPENAI_*` fallback.

Cloud wiring keeps one `DshPool` per principal; the pool reuses one runtime per
session (handshake is bounded at 30 s), caps live runtimes
(`DSH_POOL_MAX`, default 8) and reaps idle ones (`DSH_IDLE_TTL_SEC`, default 900).
Because session state lives in `DSH_HOME`, an evicted runtime resumes the same
session on the next prompt.

### 6. Tool ownership boundary (must be stated explicitly)

When `harness = "dsh"`, **dsh executes tools in its own sandbox**. The aria
`Sandbox`, `ToolExecutionPipeline`, registered `ToolHandler`s and post-processing
do **not** run for that session. What aria still owns: recall context injection,
user/assistant memory writes, graph relations, compaction, session events/titles,
approval and question bridging, and the wire envelope. Running the two tool
pipelines together would double-execute side effects and is not supported.

### 7. Deliberate deviations from dsh

- **`agentLoop` is a seam, not a bundle.** dsh calls `ctx.agentLoop` its only
  bundle (Cordis composes it). We assemble statically, so "swappable runtime"
  requires a contract.
- **`SessionEvent` payloads are mapped defensively.** Unknown frames are ignored;
  a terminal frame missing `reason.kind` is a protocol error rather than an
  invented finish reason.
- **`AGENT_LOOP` keeps its historical key name**; dsh's `agentLoop` spelling is
  accepted via `ALIASES` (`approval`, `terminals`, `spillStore`, …), and new
  dsh seam keys/service roles were added to the manifest instead of renaming.

### 8. Defaults

- **Cloud** (`aria-agent`): `AGENT_HARNESS=dsh` unless the operator set it
  (set at boot in `cmd_serve`). `native` is a supported explicit fallback.
- **Core library**: `AgentConfig::default().harness == "native"` so unit tests
  and local embeds stay hermetic.
- **Native SDK** (`ariacompute-agent`): defaults to `native`, opt-in with
  `ARIA_AGENT_HARNESS=dsh` — a mobile host ships no dsh binary, and making dsh the
  default there would break every FFI client at construction time. When `dsh` is
  selected, an unavailable runtime is an actionable error, never a downgrade.

## Consequences

- Deployments need the runtime installed: `pip install deepseek-harness-sdk`
  (pulls `deepseek-harness-runtime-bin`) or set `DSH_BIN`. Missing binary ⇒ boot
  or first-turn failure with an install hint.
- dsh must be pointed at a provider: `GATEWAY_BASE_URL` + `GATEWAY_API_KEY`
  (any OpenAI-compatible endpoint, e.g. Alibaba Model Studio), with
  `GATEWAY_MODEL` optionally pinning the model id. The deployment bridges these
  into dsh's child env names; `DSH_PROVIDER` is still needed when the provider
  is not `openai-compatible`.
- `just dsh-smoke` runs the scripted-fake-runtime tests (offline);
  `just dsh-e2e` runs one real turn against a real `dsh` and provider, gated on
  `ARIA_DSH_E2E=1`. Neither fabricates a result when the dependency is absent.
- Memory/tenancy semantics stay identical across runtimes because `DshAgentLoop`
  writes the same `SessionEvent`s and the same context fragments.
- A dsh upgrade should only require `event_map.rs` changes; method names are
  centralised in `rpc.rs` and exercised by the smoke test.
