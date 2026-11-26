//! Postgres implementation of the RSI ports.
//!
//! Two contracts live here:
//!
//! * [`PlaybookStore`] — pending proposals and published revisions. `promote`
//!   runs in a transaction: it reads the current revision, compares it with the
//!   caller's expectation, inserts `current + 1` and deletes the proposal. The
//!   unique index on `(principal_id, domain, key, revision)` is the real guard —
//!   if a concurrent promotion wins the race, the insert fails on the constraint
//!   and is reported as [`SeamError::Conflict`], never retried as an overwrite.
//! * [`RsiStore`] — diagnoses. Every query is filtered by `principal_id` unless
//!   the caller is an admin, so a tenant can never read another tenant's report.
//!
//! Excluded from the coverage gate like the rest of `store/pg*`: these paths need
//! a live database, and the same semantics are covered through the in-memory
//! implementation.

use agent_core::context::now_ms;
use agent_core::seams::{
    Playbook, PlaybookCandidate, PlaybookStore, Proposal, RsiScope, SeamError, SeamResult,
};
use agent_core::session::insight::{Evidence, Finding};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sqlx::{Executor, PgPool, Row};

use crate::auth::PrincipalKind;
use crate::store::{can_see, AppError, PgStore, ReportRow, RsiStore};
use agent_core::CoreError;

use super::Principal;

fn io(e: sqlx::Error) -> SeamError {
    SeamError::Io(format!("postgres: {e}"))
}

fn store_io(e: sqlx::Error) -> AppError {
    AppError::Db(e)
}

fn encode<T: Serialize>(value: &T, what: &str) -> SeamResult<String> {
    serde_json::to_string(value)
        .map_err(|e| SeamError::Invalid(format!("cannot encode {what}: {e}")))
}

fn decode<T: DeserializeOwned>(raw: &str, what: &str) -> SeamResult<T> {
    serde_json::from_str(raw).map_err(|e| SeamError::Invalid(format!("cannot decode {what}: {e}")))
}

fn decode_store<T: DeserializeOwned>(raw: &str, what: &str) -> Result<T, AppError> {
    serde_json::from_str(raw).map_err(|e| AppError::Core(CoreError::Config(format!("{what}: {e}"))))
}

/// Create the three RSI tables.
///
/// Mirrors `migrations/0009_rsi.sql` step by step: the migration file is what
/// `sqlx migrate run` applies, and this function is what a binary started without
/// the CLI applies at boot. Both are idempotent.
pub async fn ensure_rsi_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    pool.execute(
        "CREATE TABLE IF NOT EXISTS rsi_reports (\
            id TEXT PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            session TEXT NOT NULL, \
            score INTEGER NOT NULL, \
            truncated BOOLEAN NOT NULL DEFAULT false, \
            findings TEXT NOT NULL DEFAULT '[]', \
            created_at_ms BIGINT NOT NULL)",
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_rsi_reports_principal \
            ON rsi_reports (principal_id, created_at_ms DESC)",
    )
    .await?;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS rsi_proposals (\
            id TEXT PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            domain TEXT NOT NULL, \
            key TEXT NOT NULL, \
            title TEXT NOT NULL, \
            body TEXT NOT NULL, \
            score INTEGER NOT NULL, \
            baseline INTEGER NOT NULL, \
            evidence TEXT NOT NULL DEFAULT '[]', \
            scope TEXT NOT NULL, \
            origin TEXT NOT NULL, \
            adaptation TEXT, \
            created_at_ms BIGINT NOT NULL)",
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_rsi_proposals_principal \
            ON rsi_proposals (principal_id, domain, created_at_ms)",
    )
    .await?;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS rsi_playbooks (\
            id TEXT PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            domain TEXT NOT NULL, \
            key TEXT NOT NULL, \
            revision BIGINT NOT NULL, \
            title TEXT NOT NULL, \
            body TEXT NOT NULL, \
            score INTEGER NOT NULL, \
            source_proposal TEXT NOT NULL, \
            scope TEXT NOT NULL, \
            origin TEXT NOT NULL, \
            adaptation TEXT, \
            updated_at_ms BIGINT NOT NULL)",
    )
    .await?;
    pool.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_rsi_playbooks_revision \
            ON rsi_playbooks (principal_id, domain, key, revision)",
    )
    .await?;
    Ok(())
}

/// Split `"pathology:tool"` back into its parts.
fn split_key(key: &str) -> (String, Option<String>) {
    match key.split_once(':') {
        Some((pathology, tool)) => (pathology.to_string(), Some(tool.to_string())),
        None => (key.to_string(), None),
    }
}

fn proposal_from_candidate(candidate: &PlaybookCandidate) -> Proposal {
    let (pathology, tool) = split_key(&candidate.key);
    Proposal {
        id: candidate.proposal_id.clone(),
        scope: candidate.scope.clone(),
        origin: candidate.origin.clone(),
        pathology,
        tool,
        title: candidate.title.clone(),
        body: candidate.body.clone(),
        score: candidate.score,
        baseline: candidate.baseline,
        evidence: candidate.evidence.clone(),
        created_at_ms: now_ms(),
        adaptation: candidate.adaptation.clone(),
    }
}

fn candidate_from_row(row: &sqlx::postgres::PgRow) -> SeamResult<PlaybookCandidate> {
    let scope: RsiScope = decode(row.get::<String, _>("scope").as_str(), "scope")?;
    let origin: RsiScope = decode(row.get::<String, _>("origin").as_str(), "origin")?;
    let evidence: Vec<Evidence> = decode(row.get::<String, _>("evidence").as_str(), "evidence")?;
    let adaptation = row
        .get::<Option<String>, _>("adaptation")
        .map(|raw| decode::<agent_core::seams::Adaptation>(&raw, "adaptation"))
        .transpose()?;
    Ok(PlaybookCandidate {
        proposal_id: row.get("id"),
        origin,
        scope,
        key: row.get("key"),
        title: row.get("title"),
        body: row.get("body"),
        score: row.get("score"),
        baseline: row.get("baseline"),
        evidence,
        adaptation,
    })
}

fn playbook_from_row(row: &sqlx::postgres::PgRow) -> SeamResult<Playbook> {
    let scope: RsiScope = decode(row.get::<String, _>("scope").as_str(), "scope")?;
    let origin: RsiScope = decode(row.get::<String, _>("origin").as_str(), "origin")?;
    let adaptation = row
        .get::<Option<String>, _>("adaptation")
        .map(|raw| decode::<agent_core::seams::Adaptation>(&raw, "adaptation"))
        .transpose()?;
    Ok(Playbook {
        key: row.get("key"),
        scope,
        origin,
        revision: row.get::<i64, _>("revision") as u64,
        title: row.get("title"),
        body: row.get("body"),
        score: row.get("score"),
        source_proposal: row.get("source_proposal"),
        adaptation,
        updated_at_ms: row.get("updated_at_ms"),
    })
}

#[async_trait]
impl PlaybookStore for PgStore {
    async fn submit(&self, candidate: PlaybookCandidate) -> SeamResult<Proposal> {
        candidate.validate()?;
        let existing = sqlx::query("SELECT id FROM rsi_proposals WHERE id = $1")
            .bind(&candidate.proposal_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(io)?;
        if existing.is_some() {
            // Re-submitting is a retry: return the stored candidate unchanged.
            return Ok(proposal_from_candidate(&candidate));
        }
        sqlx::query(
            "INSERT INTO rsi_proposals \
                (id, principal_id, domain, key, title, body, score, baseline, evidence, \
                 scope, origin, adaptation, created_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(&candidate.proposal_id)
        .bind(&candidate.scope.principal)
        .bind(&candidate.scope.domain)
        .bind(&candidate.key)
        .bind(&candidate.title)
        .bind(&candidate.body)
        .bind(candidate.score)
        .bind(candidate.baseline)
        .bind(encode(&candidate.evidence, "evidence")?)
        .bind(encode(&candidate.scope, "scope")?)
        .bind(encode(&candidate.origin, "origin")?)
        .bind(match &candidate.adaptation {
            Some(adaptation) => Some(encode(adaptation, "adaptation")?),
            None => None,
        })
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .map_err(io)?;
        Ok(proposal_from_candidate(&candidate))
    }

    async fn pending(&self, scope: &RsiScope) -> SeamResult<Vec<Proposal>> {
        let rows = sqlx::query(
            "SELECT id, scope, origin, key, title, body, score, baseline, evidence, adaptation \
             FROM rsi_proposals WHERE principal_id = $1 AND domain = $2 \
             ORDER BY created_at_ms ASC, id ASC",
        )
        .bind(&scope.principal)
        .bind(&scope.domain)
        .fetch_all(&self.pool)
        .await
        .map_err(io)?;
        rows.iter()
            .map(|row| candidate_from_row(row).map(|candidate| proposal_from_candidate(&candidate)))
            .collect()
    }

    async fn promote(&self, proposal_id: &str, expected_revision: u64) -> SeamResult<Playbook> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        let row = sqlx::query(
            "SELECT id, scope, origin, key, title, body, score, baseline, evidence, adaptation \
             FROM rsi_proposals WHERE id = $1",
        )
        .bind(proposal_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(io)?
        .ok_or_else(|| SeamError::NotFound(format!("proposal {proposal_id}")))?;
        let candidate = candidate_from_row(&row)?;
        candidate.validate()?;

        let current: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(revision), 0) FROM rsi_playbooks \
             WHERE principal_id = $1 AND domain = $2 AND key = $3",
        )
        .bind(&candidate.scope.principal)
        .bind(&candidate.scope.domain)
        .bind(&candidate.key)
        .fetch_one(&mut *tx)
        .await
        .map_err(io)?;
        if current as u64 != expected_revision {
            // Dropping `tx` rolls back; the candidate stays pending.
            return Err(SeamError::Conflict(format!(
                "playbook {} is at revision {current}, promotion expected {expected_revision}",
                candidate.key
            )));
        }

        let revision = current + 1;
        let updated_at_ms = now_ms();
        let insert = sqlx::query(
            "INSERT INTO rsi_playbooks \
                (id, principal_id, domain, key, revision, title, body, score, \
                 source_proposal, scope, origin, adaptation, updated_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(format!("{proposal_id}:r{revision}"))
        .bind(&candidate.scope.principal)
        .bind(&candidate.scope.domain)
        .bind(&candidate.key)
        .bind(revision)
        .bind(&candidate.title)
        .bind(&candidate.body)
        .bind(candidate.score)
        .bind(proposal_id)
        .bind(encode(&candidate.scope, "scope")?)
        .bind(encode(&candidate.origin, "origin")?)
        .bind(match &candidate.adaptation {
            Some(adaptation) => Some(encode(adaptation, "adaptation")?),
            None => None,
        })
        .bind(updated_at_ms)
        .execute(&mut *tx)
        .await;
        match insert {
            Ok(_) => {}
            // A concurrent promotion inserted the same revision first.
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return Err(SeamError::Conflict(format!(
                    "playbook {} revision {revision} was published concurrently",
                    candidate.key
                )))
            }
            Err(e) => return Err(io(e)),
        }

        sqlx::query("DELETE FROM rsi_proposals WHERE id = $1")
            .bind(proposal_id)
            .execute(&mut *tx)
            .await
            .map_err(io)?;
        tx.commit().await.map_err(io)?;

        Ok(Playbook {
            key: candidate.key,
            scope: candidate.scope,
            origin: candidate.origin,
            revision: revision as u64,
            title: candidate.title,
            body: candidate.body,
            score: candidate.score,
            source_proposal: proposal_id.to_string(),
            adaptation: candidate.adaptation,
            updated_at_ms,
        })
    }

    async fn discard(&self, proposal_id: &str) -> SeamResult<Proposal> {
        let row = sqlx::query(
            "DELETE FROM rsi_proposals WHERE id = $1 \
             RETURNING id, scope, origin, key, title, body, score, baseline, evidence, adaptation",
        )
        .bind(proposal_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(io)?
        .ok_or_else(|| SeamError::NotFound(format!("proposal {proposal_id}")))?;
        Ok(proposal_from_candidate(&candidate_from_row(&row)?))
    }

    async fn current(&self, scope: &RsiScope, key: &str) -> SeamResult<Option<Playbook>> {
        let row = sqlx::query(
            "SELECT key, scope, origin, revision, title, body, score, source_proposal, \
                    adaptation, updated_at_ms FROM rsi_playbooks \
             WHERE principal_id = $1 AND domain = $2 AND key = $3 \
             ORDER BY revision DESC LIMIT 1",
        )
        .bind(&scope.principal)
        .bind(&scope.domain)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(io)?;
        row.as_ref().map(playbook_from_row).transpose()
    }

    async fn list_versions(&self, scope: &RsiScope, key: &str) -> SeamResult<Vec<Playbook>> {
        let rows = sqlx::query(
            "SELECT key, scope, origin, revision, title, body, score, source_proposal, \
                    adaptation, updated_at_ms FROM rsi_playbooks \
             WHERE principal_id = $1 AND domain = $2 AND key = $3 ORDER BY revision ASC",
        )
        .bind(&scope.principal)
        .bind(&scope.domain)
        .bind(key)
        .fetch_all(&self.pool)
        .await
        .map_err(io)?;
        rows.iter().map(playbook_from_row).collect()
    }
}

#[async_trait]
impl RsiStore for PgStore {
    async fn insert_report(&self, row: &ReportRow) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO rsi_reports \
                (id, principal_id, session, score, truncated, findings, created_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&row.id)
        .bind(&row.principal_id)
        .bind(&row.session)
        .bind(row.score)
        .bind(row.truncated)
        .bind(
            serde_json::to_string(&row.findings)
                .map_err(|e| AppError::Core(CoreError::Config(e.to_string())))?,
        )
        .bind(row.created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(store_io)?;
        Ok(())
    }

    async fn list_reports(
        &self,
        principal: &Principal,
        limit: i64,
    ) -> Result<Vec<ReportRow>, AppError> {
        let rows = if principal.kind == PrincipalKind::Admin {
            sqlx::query(
                "SELECT id, principal_id, session, score, truncated, findings, created_at_ms \
                 FROM rsi_reports ORDER BY created_at_ms DESC LIMIT $1",
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(store_io)?
        } else {
            sqlx::query(
                "SELECT id, principal_id, session, score, truncated, findings, created_at_ms \
                 FROM rsi_reports WHERE principal_id = $1 \
                 ORDER BY created_at_ms DESC LIMIT $2",
            )
            .bind(&principal.id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(store_io)?
        };
        rows.iter().map(report_from_row).collect()
    }

    async fn get_report(&self, principal: &Principal, id: &str) -> Result<ReportRow, AppError> {
        let row = sqlx::query(
            "SELECT id, principal_id, session, score, truncated, findings, created_at_ms \
             FROM rsi_reports WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_io)?
        .ok_or(AppError::NotFound)?;
        let report = report_from_row(&row)?;
        if !can_see(principal, &report.principal_id) {
            // Same answer as a missing row: a tenant must not learn that another
            // tenant's report id exists.
            return Err(AppError::NotFound);
        }
        Ok(report)
    }
}

fn report_from_row(row: &sqlx::postgres::PgRow) -> Result<ReportRow, AppError> {
    Ok(ReportRow {
        id: row.get("id"),
        principal_id: row.get("principal_id"),
        session: row.get("session"),
        score: row.get("score"),
        truncated: row.get("truncated"),
        findings: decode_store::<Vec<Finding>>(
            row.get::<String, _>("findings").as_str(),
            "findings",
        )?,
        created_at_ms: row.get("created_at_ms"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_split_into_pathology_and_tool() {
        assert_eq!(
            split_key("retry_storm:shell"),
            ("retry_storm".to_string(), Some("shell".to_string()))
        );
        assert_eq!(
            split_key("compaction_churn"),
            ("compaction_churn".to_string(), None)
        );
    }

    #[test]
    fn encode_and_decode_roundtrip() {
        let evidence = vec![Evidence {
            seq: 7,
            turn_id: Some("t1".into()),
            tool: Some("shell".into()),
            detail: "tool_error".into(),
        }];
        let raw = encode(&evidence, "evidence").unwrap();
        let back: Vec<Evidence> = decode(&raw, "evidence").unwrap();
        assert_eq!(back, evidence);
        assert!(matches!(
            decode::<Vec<Evidence>>("{", "evidence"),
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            decode_store::<Vec<Evidence>>("{", "evidence"),
            Err(AppError::Core(_))
        ));
    }
}
