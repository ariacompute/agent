-- Add per-tenant ownership to the `agents` metadata table so each API key's
-- principal can only see/mutate its own agents. `runs` already carried a
-- `principal_id` (audit). Idempotent; also applied inline by `ensure_schema`.

ALTER TABLE agents ADD COLUMN IF NOT EXISTS principal_id TEXT;
