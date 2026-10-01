# 0013 — Agent × Memo integration

- Status: accepted
- Date: 2026-10-01
- Related: 0004 (memo storage), 0010 (pgvector context), 0011 (memory backend switch)

## Context

`aria-agent-memo` implements the full `ContextStore` surface in
`aria-agent-core` — not just `recall`/`memorize` but also `compact`
(session summarization), `relate`/`get_relations`/`delete_relations`, and
`expand` (bounded multi-relational graph retrieval). Both the local backend
(`MemoContextStore`, aria memo / SQLite, the same `memories` schema as the
`aria-memo` CLI) and the cloud backend (`PgContextStore`, pgvector) implement it.

Until now the agentic run loop only ever called `recall` + `memorize`. The
memory graph, compaction, and the agent-driven read/write of long-term memory
were implemented but never exercised by the agent. We want the agent to use the
whole memo capability surface, and to let the model actively manage its memory.

## Decision

Wire memo into the run loop and the tool surface in three layers, all behind the
already-frozen `ContextStore` / `ToolHandler` contracts (zero new public ABI).

1. **Run-loop memory graph (core).** `AgentConfig` gains two flags:
   `memory_graph: bool` (default **false**) and `memory_compact_threshold: usize`
   (default `0` = off). When `memory_graph` is on, every turn:
   - supplements vector `recall` with a bounded `expand` (seeds = recalled
     fragment ids, `max_hops = 2`, `budget = 32`, `top_k = 8`);
   - relates the turn's user↔assistant fragments with a `RelationKind::Temporal`
     edge via `relate`;
   - `compact`s the session once its fragment count exceeds
     `memory_compact_threshold` (via `maybe_compact_store`).
   All graph/compact calls are best-effort (`let _ =` / `if let Ok`): a backend
   that lacks graph support degrades silently and never breaks a turn.

2. **Idempotent compaction.** `compact` in both `MemoContextStore` and
   `MemoryContextStore` reuses the existing `__compact__{session}` note id instead
   of minting a new one each call, so repeated compactions overwrite rather than
   accumulate a new note per turn (history stays bounded).

3. **Memo tool family (core).** `memo_tools()` builds `memo_store` / `memo_get`
   / `memo_search` / `memo_related`; `MemoToolHandler` (an `Arc<dyn ToolHandler>`)
   routes `memo_*` calls to the agent's `ContextStore`. The handler lives in
   `aria-agent-core` and only depends on `Arc<dyn ContextStore>`, mirroring
   `aria-agent-browser`'s tool family. Tool results are persisted as `ToolResult`
   fragments so later turns can recall them.

4. **SDK / FFI wiring.** `ariacompute-agent`'s `make_agent` now always builds the
   agent with `Agent::with_handlers`, injecting a `MemoToolHandler` (and, for
   `sandbox_provider = "browser"`, a `BrowserToolHandler`). It sets
   `memory_graph = true` and `memory_compact_threshold = 32` (full integration;
   core's own unit tests stay hermetic at the default `false`/`0`). `sdk_tools()`
   appends `memo_tools()`; the FFI export surface is unchanged.

## Consequences

- `aria-agent-core` still does **not** depend on `aria-agent-memo` — it depends
  only on the `ContextStore` trait. Local (SQLite) and cloud (pgvector) backends
  remain interchangeable; the cloud's `build_agent` keeps `memory_graph = false`
  (its `PgContextStore` supports it and can be enabled per-deployment).
- The agent now retrieves context both by embedding similarity and along the
  conversation graph, and can actively persist/query long-term memory.
- Added end-to-end tests (`ariacompute-agent`): a real `MemoContextStore`
  exercises cross-turn recall, memory-graph edges, the memo tools, and idempotent
  compaction. `cargo test --workspace` stays green; `cargo clippy` clean.
- Publish order unchanged (`aria-agent-memo` is published before `ariacompute-agent`;
  core still has no memo dependency).
