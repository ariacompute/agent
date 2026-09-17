-- Cloud context storage: Postgres + pgvector.
--
-- Conversational / long-term context lives here (previously in the local sled
-- memo store). `CREATE EXTENSION vector` requires the pgvector image
-- (`pgvector/pgvector:pg16`); the service refuses to start without it.
-- Idempotent: replayed on every `sqlx migrate run` / service boot.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS context_fragments (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    fragment_key TEXT,
    kind         TEXT NOT NULL,
    content      TEXT NOT NULL,
    created_at   BIGINT NOT NULL,
    embedding    vector(256)
);

-- btree pre-filter (tenant + session) before the ANN scan.
CREATE INDEX IF NOT EXISTS idx_context_fragments_tenant_session
    ON context_fragments (principal_id, session_id);

CREATE INDEX IF NOT EXISTS idx_context_fragments_kind
    ON context_fragments (principal_id, session_id, kind);

-- Approximate nearest neighbour index over the cosine distance operator.
CREATE INDEX IF NOT EXISTS idx_context_fragments_embedding
    ON context_fragments USING hnsw (embedding vector_cosine_ops);
