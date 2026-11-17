-- Agent teams: a task DAG several agents work through (`team_*` tools,
-- `/v1/agent-teams`).
--
-- The DAG lives in one JSON document (`tasks`) instead of a `team_tasks` table:
-- claims are read-modify-write of a single row, so a claim/complete pair runs
-- under one row lock and can never interleave. Validation (unknown ids,
-- duplicates, cycles) happens before the row is written; the graph never
-- changes shape after creation, only task states do.
--
-- Idempotent: replayed on every boot.

CREATE TABLE IF NOT EXISTS agent_teams (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    name         TEXT NOT NULL,
    tasks        TEXT NOT NULL DEFAULT '[]',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_agent_teams_principal
    ON agent_teams (principal_id, created_at);
