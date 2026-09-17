-- beta Agents resource: agent columns + sessions / turns / items.
--
-- Mirrors the OpenAI beta Agents surface (`/v1/agents`, `/v1/agents/sessions`).
-- Idempotent: replayed on every `sqlx migrate run` / service boot.

ALTER TABLE agents ADD COLUMN IF NOT EXISTS model TEXT;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS instructions TEXT;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS reasoning_effort TEXT;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS reasoning_summary TEXT;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS multi_agent_enabled BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS max_concurrent_subagents INTEGER;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS service_tier TEXT DEFAULT 'auto';
ALTER TABLE agents ADD COLUMN IF NOT EXISTS text_format TEXT;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS text_verbosity TEXT;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS tools TEXT NOT NULL DEFAULT '[]';
ALTER TABLE agents ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ;

CREATE TABLE IF NOT EXISTS agent_sessions (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    agent_id     TEXT NOT NULL,
    instructions TEXT,
    status       TEXT NOT NULL DEFAULT 'idle',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_agent_sessions_principal
    ON agent_sessions (principal_id, created_at);

CREATE TABLE IF NOT EXISTS session_turns (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    agent_id     TEXT NOT NULL,
    status       TEXT NOT NULL,
    output       TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_session_turns_session
    ON session_turns (principal_id, session_id);

CREATE TABLE IF NOT EXISTS session_items (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    turn_id      TEXT NOT NULL,
    item_type    TEXT NOT NULL,
    role         TEXT,
    status       TEXT NOT NULL,
    content      TEXT NOT NULL,
    seq          INTEGER NOT NULL DEFAULT 0,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_session_items_session
    ON session_items (principal_id, session_id);
