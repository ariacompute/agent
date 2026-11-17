-- Session titles: the deterministic first-prompt title plus an optional
-- provider override (`SessionEvent::TitleSet` in the runtime).
--
-- One row per session. The deterministic fallback (`first_prompt`) is written
-- once; a provider override (`provider`) may replace it. The row is a projection
-- of `SessionEvent::TitleSet`, never a second source of truth (ADR-0015).
--
-- Idempotent: replayed on every boot.

CREATE TABLE IF NOT EXISTS session_titles (
    principal_id TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    title        TEXT NOT NULL,
    source       TEXT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (principal_id, session_id)
);
