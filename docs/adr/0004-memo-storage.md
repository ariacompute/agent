# ADR 0004: memo storage (context memory, not Postgres)

## Status

**Superseded by ADR-0010 (`0010-context-storage-pgvector.md`).**

Retained for history. ADR-0010 reverses the "never Postgres" rule for the
**cloud** deployment: context now lives in Postgres + pgvector, sharded per
principal. The embedded store remains for the **on-device** SDK only.

## Context (original)

The platform needs a single, reliable source of conversational / long-term
context. The cloud service also needs a database, which tempts putting
everything in Postgres.

## Decision (original, no longer in force)

* **memo (`aria-agent-memo`) is the only context-memory store.** It uses a
  local/embedded store (sled) and **does not use Postgres**.
* **Postgres is used exclusively by `aria-agent-cloud`** for structured metadata
  (`agents`, `runs` tables). Conversational context is never persisted there.
* `aria-agent-core` reads/writes context *only* through `MemoStore`; `aria-agent-cloud`
  and `ariacompute-agent` share the same memo implementation.
* The store supports `memorize` / `recall` (keyword/vector retrieval) /
  `compact` (session summarization) and optional long-term memory by key.

## Current state

* The contract is `aria-agent-core::context::ContextStore` (formerly `MemoStore`).
* Cloud implementation: `aria-agent-cloud::context::pg::PgContextStore`
  (Postgres + pgvector, `context_fragments`).
* On-device implementation: `aria-agent-memo::SledContextStore` (sled).
* `AGENT_MEMO_BACKEND` / `MEMO_DIR` are gone; the compose image provides pgvector.
