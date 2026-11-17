# Release Notes ✨

## 🧩 Capability seams in, vendored harness out 🚀

A short, scannable version of the release notes.

### 🧩 New kernel

- 🔌 Every capability (model, sandbox, storage, sessions, tools, approvals,
  sub-agents, scheduling) is now a seam with a replaceable provider.
- 🧱 `aria-agent-plugin` validates the plugin graph at boot: no duplicate ids,
  no conflicting `core` providers, no cycles, no missing dependencies, then
  freezes it — `Agent` resolves every seam once, no lookups on the hot path.
- 🛠️ One tool pipeline: resolve → preset → approval → dispatch → truncate/spill.
  🚫 No approval provider means refused, never silently allowed.

### 💬 Sessions & titles

- 📜 `SessionEvent` is the single source of truth; batch-flushed JSONL locally,
  a projection in Postgres. 📊 Provider token usage wins, local estimates only
  fill the gap; overflow compacts once and retries once.
- 🏷️ Deterministic first-prompt title + optional async provider override. All
  three loops (`run`, `run_stream`, `run_event_stream`) record and flush it
  before the model call.
- 👶 Child sessions (`<parent>::sub::<id>`) are titled and addressable through
  `GET /v1/agents/sessions/{id}/title`, authorised via their parent tenant.

### 🤖 Orchestration

- 🧒 In-process sub-agents with namespaced sessions, a concurrency cap and
  cached `wait` (delegation depth is one in the cloud).
- 🗂️ Agent teams are validated task DAGs: cycles, duplicate and dangling ids
  are refused; claims are dependency-gated.
- ⏰ Scheduled jobs advance `next_run_ms` before running, so a restart or a
  second replica cannot fire the same instant twice.

### ☁️ Platform & SDK

- 🗄️ New idempotent, `principal_id`-scoped tables: `scheduled_jobs`,
  `agent_teams`, `session_titles`; new `/v1/agent-teams`, `/v1/scheduled-jobs`
  and `GET …/title` routes.
- 📡 New SSE frames: `agent.turn.approval.*` and `agent.turn.usage` — existing
  frames and terminal semantics unchanged.
- 📱 `SdkAgentConfig` gains `permission_preset` / `approval_mode`; Swift/Kotlin
  bindings regenerated, JS/Python expose `result.usage`.

### ⚠️ Breaking

- 🗑️ The `codex` submodule, its git deps, exact `rama-*` pins, the patch section,
  `CodexSandbox` and `SandboxProvider::Codex` are gone.
- 🛑 `sandbox_provider = "codex"` now fails at startup; docker is the default.
- 🔒 Deployments without an approval broker refuse mutations instead of
  implicitly allowing them.

### ✅ Validation

`cargo test --workspace` (563 ✅), JS SDK (53 ✅), Python SDK (30 ✅), plus
`fmt`, `clippy -D warnings` and `just ffi` all green.
