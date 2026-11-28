-- A2A tasks and their coarse-grained event log (ADR-0028).
--
-- `context_id` mirrors `agent_sessions.id` and `turn_id` mirrors `session_turns.id`,
-- so an A2A conversation shows up in the existing console. Token deltas are
-- *not* stored here: the event log keeps coarse frames (task, status, artifact,
-- message) and the final text, which is what a reconnect replays.

CREATE TABLE IF NOT EXISTS a2a_tasks (
    task_id TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    context_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    state TEXT NOT NULL,
    status_message TEXT,
    artifacts TEXT NOT NULL DEFAULT '[]',
    push_config TEXT,
    push_last TEXT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_a2a_tasks_principal
    ON a2a_tasks (principal_id, task_id);

CREATE INDEX IF NOT EXISTS idx_a2a_tasks_context
    ON a2a_tasks (principal_id, context_id, created_at_ms);

CREATE TABLE IF NOT EXISTS a2a_task_events (
    task_id TEXT NOT NULL,
    seq BIGINT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (task_id, seq)
);
