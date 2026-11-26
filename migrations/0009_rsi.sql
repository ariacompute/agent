-- Recursive self-improvement (RSI): diagnoses, pending proposals, versioned
-- playbooks (ADR-0026).
--
-- Four tables, each with one job:
--
--   rsi_reports   — the durable artifact of a diagnosis. `findings` is a JSON
--                   array of `{pathology, tool, occurrences, evidence:[{seq,…}]}`,
--                   so a claim can always be traced back to ledger positions.
--   rsi_proposals — a validated candidate that is **not** in force. A row here
--                   means "a human (or the configured gate) still has to decide".
--   rsi_playbooks — published revisions. The unique index on
--                   (principal_id, domain, key, revision) is what makes the
--                   revision guard real: a racing promotion fails on the
--                   constraint instead of overwriting a reviewer's work.
--   rsi_reruns    — one receipt per real re-run: the only RSI step that spends
--                   model calls. Insert-only, because cost and side effects must
--                   not be rewritable after the fact.
--
-- `scope` / `origin` hold the serialized RsiScope (principal, domain, params) and
-- `principal_id` / `domain` are kept alongside as indexed projections of them.
--
-- Idempotent: replayed on every boot.

CREATE TABLE IF NOT EXISTS rsi_reports (
    id            TEXT PRIMARY KEY,
    principal_id  TEXT NOT NULL,
    session       TEXT NOT NULL,
    score         INTEGER NOT NULL,
    truncated     BOOLEAN NOT NULL DEFAULT false,
    findings      TEXT NOT NULL DEFAULT '[]',
    created_at_ms BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rsi_reports_principal
    ON rsi_reports (principal_id, created_at_ms DESC);

CREATE TABLE IF NOT EXISTS rsi_proposals (
    id            TEXT PRIMARY KEY,
    principal_id  TEXT NOT NULL,
    domain        TEXT NOT NULL,
    key           TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    score         INTEGER NOT NULL,
    baseline      INTEGER NOT NULL,
    evidence      TEXT NOT NULL DEFAULT '[]',
    scope         TEXT NOT NULL,
    origin        TEXT NOT NULL,
    adaptation    TEXT,
    created_at_ms BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rsi_proposals_principal
    ON rsi_proposals (principal_id, domain, created_at_ms);

CREATE TABLE IF NOT EXISTS rsi_playbooks (
    id              TEXT PRIMARY KEY,
    principal_id    TEXT NOT NULL,
    domain          TEXT NOT NULL,
    key             TEXT NOT NULL,
    revision        BIGINT NOT NULL,
    title           TEXT NOT NULL,
    body            TEXT NOT NULL,
    score           INTEGER NOT NULL,
    source_proposal TEXT NOT NULL,
    scope           TEXT NOT NULL,
    origin          TEXT NOT NULL,
    adaptation      TEXT,
    updated_at_ms   BIGINT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_rsi_playbooks_revision
    ON rsi_playbooks (principal_id, domain, key, revision);

CREATE TABLE IF NOT EXISTS rsi_reruns (
    id                TEXT PRIMARY KEY,
    principal_id      TEXT NOT NULL,
    domain            TEXT NOT NULL,
    proposal_id       TEXT NOT NULL,
    key               TEXT NOT NULL,
    baseline          INTEGER NOT NULL,
    score             INTEGER NOT NULL,
    verdict           TEXT NOT NULL,
    prompt_tokens     BIGINT NOT NULL,
    completion_tokens BIGINT NOT NULL,
    steps             INTEGER NOT NULL,
    tool_calls        INTEGER NOT NULL,
    duration_ms       BIGINT NOT NULL,
    note              TEXT NOT NULL DEFAULT '',
    created_at_ms     BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rsi_reruns_principal
    ON rsi_reruns (principal_id, created_at_ms DESC);

CREATE INDEX IF NOT EXISTS idx_rsi_reruns_proposal
    ON rsi_reruns (proposal_id, created_at_ms DESC);
