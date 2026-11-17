# ADR-0015 — Session event stream, persistence, usage and compaction

Status: Accepted.

## Context

The loop emitted `AgentEvent`s for streaming, wrote turns/items into Postgres, and
kept a separate in-memory transcript for the model. Three representations of one
conversation drifted: a title could exist in one place, token counts in another, and
local (JSONL) and cloud (rows) consumers saw different vocabularies.

## Decision

* `SessionEvent` is the **only** vocabulary, append-only:
  `turn_started`, `step`, `item_added`, `item_done`, `text_delta`, `text_done`,
  `approval`, `usage`, `compacted`, `title_set`, `turn_completed`, `turn_failed`.
* Every entry point records the same vocabulary: the agentic loop
  (`run_event_stream`), the one-shot turn (`run`) and the plain stream
  (`run_stream`) all open with `turn_started` + the title and close with
  `turn_completed` / `turn_failed`. The title batch is flushed *before* the model
  call so a slow or failing provider still leaves a titled session.
* Each record carries `seq` (monotonic) and `at_ms`. `SessionRecorder` buffers one
  turn and flushes once (turn end / terminal event); a failed flush **keeps** the
  records so the next flush retries instead of silently losing the audit trail.
* Persistence is a seam. Local: one JSONL file per session (append, O(1); replay,
  O(n); a corrupt line is skipped with a warning). Cloud: the same events are
  projected into `session_turns` / `session_items` in one transaction per turn —
  the tables are a **projection**, never a second source of truth.
* `SessionProjection::replay` folds the stream into turns, items, title, todos,
  plan, approvals, compactions and usage. Query is a filter (`session`, `turn`,
  `item`, `kind`, `from_seq`, `limit`) over the stream; `from_seq` is the cursor.
* Usage: prefer the provider's `TokenUsage` (input/output/total/model). Estimate
  locally only when the backend reports nothing. `TokenMeter` tracks cumulative
  usage and pressure (`context_limit × threshold`); crossing it compacts before
  the next model call. If the provider rejects a request with a context-overflow
  error, the loop compacts **once**, re-recalls a smaller context, and retries
  **once**; a second overflow fails the turn.
* Title: the deterministic fallback (first input, trimmed/truncated) is recorded
  first (`source: first_prompt`), then an optional async provider may overwrite it
  (`source: provider`). A provider that declines never blocks a turn. In the
  cloud the projection writes `first_prompt` only when the session has no title
  yet (each turn rebuilds the agent, so the fallback would otherwise repeat),
  while a provider override always wins.
* System prompt is assembled from ordered sections (base identity → instructions →
  skills catalog → plan → working directory → permissions) with a stable sort and a
  length budget, so prompt prefixes stay cache-friendly.

## Consequences

* One wire vocabulary for SSE, JSONL, cloud rows and both SDKs.
* Local replay is enough to rebuild a session; the cloud can lose rows without
  losing semantics (and vice versa).
* Compaction is observable (`compacted` events) instead of hidden.
* Usage is honest: reported tokens are never overwritten by estimates.
