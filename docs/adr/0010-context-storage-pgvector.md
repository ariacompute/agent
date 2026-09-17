# ADR-0010 — Context storage: Postgres + pgvector (cloud), embedded store (on-device)

* Status: Accepted
* Date: 2026-09-17
* Supersedes: ADR-0004 (`0004-memo-storage.md`), which mandated that context
  memory must never live in Postgres.

## Context

ADR-0004 made the embedded sled store (`aria-agent-memo`) the only home for
conversational / long-term context, and Postgres the home of metadata only. In
practice this split hurt the cloud deployment: context was ephemeral
(`AGENT_MEMO_BACKEND=memory`) or pinned to a volume, could not be shared across
instances, and semantic recall had to scan every fragment in-process.

## Decision

1. **The context contract moves into `aria-agent-core::context`.**
   `ContextStore` (`memorize` / `recall` / `compact` / `get_by_key`),
   `ContextFragment`, `FragmentKind`, `RecallQuery`, the local hashing-trick
   `LocalEmbedder` and the keyword+vector ranking helpers now live in core, so
   every backend shares one semantic contract.
2. **The cloud stores context in Postgres + pgvector.** Table
   `context_fragments(principal_id, session_id, fragment_key, kind, content,
   created_at, embedding vector(256))` with a btree `(principal_id, session_id)`
   pre-filter and an HNSW `vector_cosine_ops` index.
3. **pgvector is a hard requirement.** Boot runs `CREATE EXTENSION IF NOT EXISTS
   vector` and then verifies `pg_extension`; if the extension is missing the
   service exits with an explicit error. There is deliberately **no** keyword-only
   fallback. Deployments must use an image that ships pgvector
   (`pgvector/pgvector:pg16`).
4. **Recall is a hybrid.** Candidate fragments come from pgvector KNN
   (`embedding <=> $q`) and from a keyword/exact-key match (`strpos`, no LIKE
   wildcard injection); both sets are re-ranked in Rust with the shared
   `0.7 * cosine + 0.3 * keyword` blend and truncated to `top_k`. An empty query
   recalls nothing.
5. **Tenancy is part of every predicate.** One `PgContextStore` per principal;
   `principal_id` is in every `WHERE` clause, so tenants cannot read each other's
   context.
6. **The on-device SDK keeps the embedded store.** `aria-agent-memo` now only
   implements the shared contract with sled and depends on `aria-agent-core`
   (dependency direction: memo → core). Native apps stay offline-first; the cloud
   never uses it.

## Consequences

* Cloud context survives restarts and can be shared by multiple instances.
* The `AGENT_MEMO_BACKEND` / `MEMO_DIR` environment variables are gone; the
  compose Postgres image becomes `pgvector/pgvector:pg16`.
* Embedding width is fixed at `EMBED_DIM = 256`; changing it requires a
  migration that rewrites the column and re-embeds every row.
* DB-backed tests are gated on `DATABASE_URL` (skipped otherwise) — no fake
  scores, no fake vectors.
