# ADR 0004: memo storage (context memory, not Postgres)

## Status
Accepted.

## Context
The platform needs a single, reliable source of conversational / long-term
context. The cloud service also needs a database, which tempts putting
everything in Postgres.

## Decision
* **memo (`agent-memo`) is the only context-memory store.** It uses a
  local/embedded store (sled) and **does not use Postgres**.
* **Postgres is used exclusively by `agent-cloud`** for structured metadata
  (`agents`, `runs` tables). Conversational context is never persisted there.
* `agent-core` reads/writes context *only* through `MemoStore`; `agent-cloud`
  and `agent-sdk` share the same memo implementation.
* The store supports `memorize` / `recall` (keyword/vector retrieval) /
  `compact` (session summarization) and optional long-term memory by key.

## Consequences
* Context survives independently of the cloud database; the cloud can be torn
  down/rebuilt without losing agent memory (subject to memo store location).
* Sensitive conversation text is kept out of the relational metadata DB.
