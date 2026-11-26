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
//! * [`RsiStore`] — diagnoses and real re-run receipts. Every query is filtered
//!   by `principal_id` unless the caller is an admin, so a tenant can never read
//!   another tenant's report; receipts are insert-only.
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
use crate::store::{can_see, AppError, PgStore, ReportRow, RerunRow, RsiStore};
use agent_core::CoreError;

use super::Principal;

fn io(e: sqlx::Error) -> SeamError {
    SeamError::Io(format!("postgres: {e}"))
}

// Cold path: keep it out of the hot `promote` body.
#[inline(never)]
fn insert_error(e: sqlx::Error, key: &str, revision: i64) -> SeamError {
    // A concurrent promotion that already claimed this revision trips the
    // unique index `(principal_id, domain, key, revision)` and must be reported
    // as a conflict; everything else is storage. The mapping lives here (not
    // inline in `promote`) so the branch can be exercised with a fabricated
    // `DatabaseError`.
    match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => SeamError::Conflict(format!(
            "playbook {key} revision {revision} was published concurrently"
        )),
        _ => io(e),
    }
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

/// Create the four RSI tables.
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
    // Real re-runs: the receipt for the only step that spends model calls.
    pool.execute(
        "CREATE TABLE IF NOT EXISTS rsi_reruns (\
            id TEXT PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            domain TEXT NOT NULL, \
            proposal_id TEXT NOT NULL, \
            key TEXT NOT NULL, \
            baseline INTEGER NOT NULL, \
            score INTEGER NOT NULL, \
            verdict TEXT NOT NULL, \
            prompt_tokens BIGINT NOT NULL, \
            completion_tokens BIGINT NOT NULL, \
            steps INTEGER NOT NULL, \
            tool_calls INTEGER NOT NULL, \
            duration_ms BIGINT NOT NULL, \
            note TEXT NOT NULL DEFAULT '', \
            created_at_ms BIGINT NOT NULL)",
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_rsi_reruns_principal \
            ON rsi_reruns (principal_id, created_at_ms DESC)",
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_rsi_reruns_proposal \
            ON rsi_reruns (proposal_id, created_at_ms DESC)",
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
        if let Err(e) = insert {
            return Err(insert_error(e, &candidate.key, revision));
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

    async fn insert_rerun(&self, row: &RerunRow) -> Result<(), AppError> {
        // Insert-only: a receipt is never amended, so cost and effects cannot be
        // rewritten after the fact.
        sqlx::query(
            "INSERT INTO rsi_reruns \
                (id, principal_id, domain, proposal_id, key, baseline, score, verdict, \
                 prompt_tokens, completion_tokens, steps, tool_calls, duration_ms, note, \
                 created_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(&row.id)
        .bind(&row.principal_id)
        .bind(&row.domain)
        .bind(&row.proposal_id)
        .bind(&row.key)
        .bind(row.baseline)
        .bind(row.score)
        .bind(&row.verdict)
        .bind(row.prompt_tokens as i64)
        .bind(row.completion_tokens as i64)
        .bind(i32::try_from(row.steps).unwrap_or(i32::MAX))
        .bind(i32::try_from(row.tool_calls).unwrap_or(i32::MAX))
        .bind(row.duration_ms as i64)
        .bind(&row.note)
        .bind(row.created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(store_io)?;
        Ok(())
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

    // ---------------------------------------------------------------- DB tests

    use agent_core::session::insight::Pathology;

    async fn store() -> Option<PgStore> {
        crate::pg_testkit::maybe_pool().await.map(PgStore::new)
    }

    /// An admin list reads *every* tenant, so the row a test deliberately
    /// corrupts must not exist while another test is listing. Both sides take
    /// this lock instead of relying on scheduling luck.
    static CORRUPTION_WINDOW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn scope() -> RsiScope {
        RsiScope::new(format!("rsi-{}", uuid::Uuid::new_v4().simple()), "support")
    }

    fn candidate(scope: &RsiScope, proposal_id: &str, key: &str) -> PlaybookCandidate {
        PlaybookCandidate {
            proposal_id: proposal_id.into(),
            origin: scope.clone(),
            scope: scope.clone(),
            key: key.into(),
            title: "Retry with a backoff".into(),
            body: "When a tool fails twice, wait 500ms before the third attempt.".into(),
            score: 60,
            baseline: 40,
            evidence: vec![Evidence {
                seq: 3,
                turn_id: Some("turn-1".into()),
                tool: Some("shell".into()),
                detail: "tool_error".into(),
            }],
            adaptation: None,
        }
    }

    #[tokio::test]
    async fn proposals_promote_into_versioned_playbooks() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let scope = scope();
        let first = candidate(
            &scope,
            &format!("p-{}", uuid::Uuid::new_v4()),
            "retry_storm:shell",
        );
        let id = first.proposal_id.clone();

        let submitted = store.submit(first.clone()).await.expect("submit");
        assert_eq!(submitted.pathology, "retry_storm");
        assert_eq!(submitted.tool.as_deref(), Some("shell"));
        // Re-submitting the same id is a retry, not a second row.
        store.submit(first.clone()).await.expect("resubmit");

        let pending = store.pending(&scope).await.expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);

        let promoted = store.promote(&id, 0).await.expect("promote r1");
        assert_eq!(promoted.revision, 1);
        assert_eq!(promoted.key, "retry_storm:shell");
        assert!(store.pending(&scope).await.expect("pending").is_empty());

        let current = store
            .current(&scope, "retry_storm:shell")
            .await
            .expect("current")
            .expect("playbook");
        assert_eq!(current.revision, 1);

        // A proposal whose expected revision is stale is refused and stays pending.
        let second = candidate(
            &scope,
            &format!("p-{}", uuid::Uuid::new_v4()),
            "retry_storm:shell",
        );
        let second_id = second.proposal_id.clone();
        store.submit(second).await.expect("submit second");
        match store.promote(&second_id, 0).await {
            Err(SeamError::Conflict(message)) => assert!(message.contains("expected 0")),
            other => panic!("expected a conflict, got {other:?}"),
        }
        let promoted = store.promote(&second_id, 1).await.expect("promote r2");
        assert_eq!(promoted.revision, 2);
        assert_eq!(
            store
                .list_versions(&scope, "retry_storm:shell")
                .await
                .expect("versions")
                .len(),
            2
        );

        assert!(matches!(
            store.promote("ghost", 0).await,
            Err(SeamError::NotFound(_))
        ));
        assert!(matches!(
            store.discard("ghost").await,
            Err(SeamError::NotFound(_))
        ));
        assert!(store
            .current(&scope, "never-published")
            .await
            .expect("current")
            .is_none());

        // Discarding deletes the pending row and hands it back.
        let third = candidate(
            &scope,
            &format!("p-{}", uuid::Uuid::new_v4()),
            "retry_storm",
        );
        let third_id = third.proposal_id.clone();
        store.submit(third).await.expect("submit third");
        let discarded = store.discard(&third_id).await.expect("discard");
        assert_eq!(discarded.id, third_id);
        // A key without a tool keeps `tool = None`.
        assert_eq!(discarded.tool, None);
        assert!(store.pending(&scope).await.expect("pending").is_empty());
    }

    /// Cross-tenant promotion without an adaptation record never reaches SQL.
    #[tokio::test]
    async fn an_unadapted_cross_tenant_proposal_is_refused() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let scope = scope();
        let mut cross = candidate(
            &scope,
            &format!("p-{}", uuid::Uuid::new_v4()),
            "retry_storm",
        );
        cross.origin = RsiScope::new("another-tenant", "billing");
        assert!(matches!(
            store.submit(cross.clone()).await,
            Err(SeamError::Unadapted(_))
        ));

        // With an adaptation record the same candidate is accepted.
        cross.adaptation = Some(agent_core::seams::Adaptation {
            from_principal: "another-tenant".into(),
            from_domain: "billing".into(),
            to_principal: scope.principal.clone(),
            to_domain: scope.domain.clone(),
            rewritten: vec!["threshold".into()],
            rationale: "the support queue uses the same thresholds".into(),
        });
        store.submit(cross).await.expect("adapted submit");
    }

    /// Rows written by an older binary must surface as seam errors, not panics.
    #[tokio::test]
    async fn corrupt_proposal_and_playbook_rows_surface_as_seam_errors() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let scope = scope();
        let id = format!("p-{}", uuid::Uuid::new_v4());
        sqlx::query(
            "INSERT INTO rsi_proposals (id, principal_id, domain, key, title, body, score, \
                baseline, evidence, scope, origin, adaptation, created_at_ms) \
             VALUES ($1, $2, $3, 'k', 't', 'b', 60, 40, '[]', '{oops', '{}', NULL, 1)",
        )
        .bind(&id)
        .bind(&scope.principal)
        .bind(&scope.domain)
        .execute(store.pool())
        .await
        .expect("insert corrupt proposal");

        assert!(matches!(
            store.pending(&scope).await,
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            store.promote(&id, 0).await,
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            store.discard(&id).await,
            Err(SeamError::Invalid(_))
        ));

        // A corrupt adaptation column takes the same route.
        let adapted = format!("p-{}", uuid::Uuid::new_v4());
        sqlx::query(
            "INSERT INTO rsi_proposals (id, principal_id, domain, key, title, body, score, \
                baseline, evidence, scope, origin, adaptation, created_at_ms) \
             VALUES ($1, $2, $3, 'k', 't', 'b', 60, 40, '[]', '{}', '{}', '{oops', 1)",
        )
        .bind(&adapted)
        .bind(&scope.principal)
        .bind(&scope.domain)
        .execute(store.pool())
        .await
        .expect("insert corrupt adaptation");
        assert!(matches!(
            store.pending(&scope).await,
            Err(SeamError::Invalid(_))
        ));

        // The same for a published playbook.
        sqlx::query(
            "INSERT INTO rsi_playbooks (id, principal_id, domain, key, revision, title, body, \
                score, source_proposal, scope, origin, adaptation, updated_at_ms) \
             VALUES ($1, $2, $3, 'retry_storm:shell', 1, 't', 'b', 60, 'p', '{oops', '{}', NULL, 1)",
        )
        .bind(format!("pb-{}", uuid::Uuid::new_v4()))
        .bind(&scope.principal)
        .bind(&scope.domain)
        .execute(store.pool())
        .await
        .expect("insert corrupt playbook");
        assert!(matches!(
            store.current(&scope, "retry_storm:shell").await,
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            store.list_versions(&scope, "retry_storm:shell").await,
            Err(SeamError::Invalid(_))
        ));
    }

    fn principal(id: &str, kind: PrincipalKind) -> Principal {
        Principal {
            id: id.to_string(),
            kind,
            scopes: crate::auth::ApiKeyScopes::read_write(),
        }
    }

    fn report(principal_id: &str) -> ReportRow {
        ReportRow {
            id: format!("rep-{}", uuid::Uuid::new_v4()),
            principal_id: principal_id.to_string(),
            session: "sess-1".into(),
            score: 55,
            truncated: false,
            findings: vec![Finding {
                pathology: Pathology::RetryStorm,
                tool: Some("shell".into()),
                occurrences: 3,
                evidence: vec![Evidence {
                    seq: 4,
                    turn_id: None,
                    tool: Some("shell".into()),
                    detail: "retry_storm".into(),
                }],
            }],
            created_at_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn reports_are_tenant_scoped_and_decode_their_findings() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let tenant = format!("rsi-{}", uuid::Uuid::new_v4().simple());
        let row = report(&tenant);
        store.insert_report(&row).await.expect("insert");

        let user = principal(&tenant, PrincipalKind::User);
        let mine = store.list_reports(&user, 10).await.expect("list");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].findings, row.findings);
        assert_eq!(
            store.get_report(&user, &row.id).await.expect("get").id,
            row.id
        );

        // Another tenant sees neither the list entry nor the row.
        let other = principal("someone-else", PrincipalKind::User);
        assert!(store
            .list_reports(&other, 10)
            .await
            .expect("list")
            .is_empty());
        assert!(matches!(
            store.get_report(&other, &row.id).await,
            Err(AppError::NotFound)
        ));
        assert!(matches!(
            store.get_report(&user, "ghost").await,
            Err(AppError::NotFound)
        ));

        // Admins read across tenants (no corrupted row may exist right now).
        let _guard = CORRUPTION_WINDOW.lock().await;
        let admin = principal("root", PrincipalKind::Admin);
        assert!(store
            .list_reports(&admin, 10)
            .await
            .expect("admin list")
            .iter()
            .any(|r| r.id == row.id));
        assert_eq!(
            store.get_report(&admin, &row.id).await.expect("get").id,
            row.id
        );
    }

    #[tokio::test]
    async fn corrupt_findings_are_a_config_error() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let _guard = CORRUPTION_WINDOW.lock().await;
        let tenant = format!("rsi-{}", uuid::Uuid::new_v4().simple());
        let row = report(&tenant);
        store.insert_report(&row).await.expect("insert");
        sqlx::query("UPDATE rsi_reports SET findings = '{oops' WHERE id = $1")
            .bind(&row.id)
            .execute(store.pool())
            .await
            .expect("corrupt findings");
        let user = principal(&tenant, PrincipalKind::User);
        assert!(matches!(
            store.get_report(&user, &row.id).await,
            Err(AppError::Core(_))
        ));
        assert!(matches!(
            store.list_reports(&user, 10).await,
            Err(AppError::Core(_))
        ));
        // Leave the table clean for the admin list above.
        sqlx::query("DELETE FROM rsi_reports WHERE id = $1")
            .bind(&row.id)
            .execute(store.pool())
            .await
            .expect("clean up");
    }

    #[tokio::test]
    async fn rerun_receipts_are_insert_only() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let row = RerunRow {
            id: format!("rerun-{}", uuid::Uuid::new_v4()),
            principal_id: format!("rsi-{}", uuid::Uuid::new_v4().simple()),
            domain: "support".into(),
            proposal_id: "p1".into(),
            key: "retry_storm:shell".into(),
            baseline: 40,
            score: 60,
            verdict: "passed".into(),
            prompt_tokens: 120,
            completion_tokens: 30,
            // Wider than a Postgres INTEGER: stored as I32::MAX, never a panic.
            steps: u32::MAX,
            tool_calls: 0,
            duration_ms: 900,
            note: String::new(),
            created_at_ms: 1_700_000_000_000,
        };
        store.insert_rerun(&row).await.expect("insert");
        // The receipt is never amended: a second insert with the same id fails.
        assert!(store.insert_rerun(&row).await.is_err());
    }

    /// A concurrent promotion that wins the unique index must read as a
    /// conflict. The branch cannot be produced deterministically against a real
    /// server, so the mapping is exercised with a fabricated `DatabaseError`.
    #[test]
    fn a_unique_violation_is_reported_as_a_conflict() {
        #[derive(Debug)]
        struct Duplicate;

        impl std::fmt::Display for Duplicate {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("duplicate key value violates unique constraint")
            }
        }

        impl std::error::Error for Duplicate {}

        impl sqlx::error::DatabaseError for Duplicate {
            fn message(&self) -> &str {
                "duplicate key value violates unique constraint"
            }
            fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
                self
            }
            fn kind(&self) -> sqlx::error::ErrorKind {
                sqlx::error::ErrorKind::UniqueViolation
            }
        }

        let conflict = insert_error(
            sqlx::Error::Database(Box::new(Duplicate)),
            "retry_storm:shell",
            3,
        );
        match conflict {
            SeamError::Conflict(message) => {
                assert!(message.contains("revision 3"));
                assert!(message.contains("concurrently"));
            }
            other => panic!("expected a conflict, got {other:?}"),
        }

        let other = insert_error(sqlx::Error::RowNotFound, "k", 1);
        assert!(matches!(other, SeamError::Io(_)));
    }
}
