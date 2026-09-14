-- agent-cloud multi-tenant API keys (Postgres metadata only).
-- API key *hashes* live here; the plaintext key is shown only once at creation.
-- Postgres holds metadata only (see AGENTS.md rule #1); tenant context/records
-- remain in local sled stores sharded by principal_id.

CREATE TABLE IF NOT EXISTS api_keys (
    id           TEXT PRIMARY KEY,
    key_prefix   TEXT NOT NULL,
    key_hash     TEXT NOT NULL UNIQUE,
    principal_id TEXT NOT NULL,
    label        TEXT,
    scopes       TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at   TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys (key_hash);

-- Attribute runs to the calling principal (audit; tenant context is in sled).
ALTER TABLE runs ADD COLUMN IF NOT EXISTS principal_id TEXT;
