-- agent-cloud metadata schema (Postgres ONLY).
-- Conversational context is intentionally NOT stored here; it lives in
-- `agent-memo` (local/embedded store). Postgres holds structured metadata.

CREATE TABLE IF NOT EXISTS agents (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS runs (
    id          TEXT PRIMARY KEY,
    agent_id    TEXT NOT NULL REFERENCES agents(id),
    session     TEXT NOT NULL,
    input       TEXT NOT NULL,
    output      TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_runs_agent_id ON runs(agent_id);
CREATE INDEX IF NOT EXISTS idx_runs_session ON runs(session);
