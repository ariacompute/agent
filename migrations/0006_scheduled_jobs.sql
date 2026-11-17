-- Scheduled jobs (`schedule_*` tools, `/v1/scheduled-jobs`).
--
-- One row per job; `next_run_ms` is the claim/cursor: the ticker advances it
-- **before** running the job, so a restart or a second replica can never fire
-- the same instant twice. Timestamps are epoch milliseconds (the wire unit the
-- orchestrator uses), `created_at` stays TIMESTAMPTZ like every other table.
--
-- Idempotent: replayed on every boot, so no `ALTER` after the fact.

CREATE TABLE IF NOT EXISTS scheduled_jobs (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    name         TEXT NOT NULL,
    every_secs   BIGINT,
    at_ms        BIGINT,
    payload      TEXT NOT NULL DEFAULT 'null',
    enabled      BOOLEAN NOT NULL DEFAULT true,
    next_run_ms  BIGINT,
    last_run_ms  BIGINT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Exactly one trigger kind: an interval or a single instant.
    CONSTRAINT scheduled_jobs_one_trigger
        CHECK ((every_secs IS NULL) <> (at_ms IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_principal
    ON scheduled_jobs (principal_id, created_at);

-- The ticker only ever scans enabled rows with a due cursor.
CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_due
    ON scheduled_jobs (next_run_ms)
    WHERE enabled;
