//! Postgres implementation of [`crate::governance::GovernanceStore`].
//!
//! Every statement filters on `principal_id`; there is no unscoped read path.
//! Policy writes use an `ON CONFLICT ... WHERE revision = $n` guard, so a stale
//! writer updates zero rows and the caller gets a conflict instead of a silent
//! overwrite.
//!
//! DB-backed behaviour is exercised by tests gated on `DATABASE_URL` (they skip
//! when it is unset — never faked).

use crate::governance::GovernanceStore;
use crate::{AppError, AppResult};
use agent_core::seams::{
    AuditEvent, AuditKind, BilledTokens, EgressMode, EgressPolicy, HostPattern, ResourceUsage,
    UsageRecord, UsageSummary,
};
use agent_sandbox::CheckpointRef;
use sqlx::{PgPool, Row};

#[derive(Clone)]
pub struct PgGovernanceStore {
    pool: PgPool,
}

impl PgGovernanceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn hosts_to_json(hosts: &[HostPattern]) -> String {
    serde_json::to_string(&hosts.iter().map(|h| h.as_str()).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".into())
}

fn hosts_from_json(raw: &str) -> AppResult<Vec<HostPattern>> {
    let names: Vec<String> = serde_json::from_str(raw)
        .map_err(|e| AppError::BadRequest(format!("stored policy hosts: {e}")))?;
    names
        .into_iter()
        .map(|h| HostPattern::parse(&h).map_err(|e| AppError::BadRequest(format!("{h}: {e}"))))
        .collect()
}

#[async_trait::async_trait]
impl GovernanceStore for PgGovernanceStore {
    async fn policy(&self, principal_id: &str) -> AppResult<EgressPolicy> {
        let row = sqlx::query(
            "SELECT revision, mode, allowed, denied FROM egress_policies WHERE principal_id = $1",
        )
        .bind(principal_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(EgressPolicy::deny_all());
        };
        let mode_raw: String = row.try_get("mode")?;
        let mode = EgressMode::parse(&mode_raw)
            .ok_or_else(|| AppError::BadRequest(format!("stored egress mode: {mode_raw}")))?;
        let revision: i64 = row.try_get("revision")?;
        Ok(EgressPolicy::new(
            revision.max(0) as u64,
            mode,
            hosts_from_json(&row.try_get::<String, _>("allowed")?)?,
            hosts_from_json(&row.try_get::<String, _>("denied")?)?,
        ))
    }

    async fn replace_policy(
        &self,
        principal_id: &str,
        expected_revision: Option<u64>,
        next: EgressPolicy,
    ) -> AppResult<EgressPolicy> {
        next.validate()
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        let allowed = hosts_to_json(&next.allowed);
        let denied = hosts_to_json(&next.denied);
        let mode = next.mode.as_str();
        let affected = match expected_revision {
            None => sqlx::query(
                "INSERT INTO egress_policies (principal_id, revision, mode, allowed, denied) \
                 VALUES ($1, 1, $2, $3, $4) \
                 ON CONFLICT (principal_id) DO NOTHING",
            )
            .bind(principal_id)
            .bind(mode)
            .bind(&allowed)
            .bind(&denied)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            Some(expected) => sqlx::query(
                "UPDATE egress_policies \
                    SET revision = revision + 1, mode = $2, allowed = $3, denied = $4, \
                        updated_at = now() \
                  WHERE principal_id = $1 AND revision = $5",
            )
            .bind(principal_id)
            .bind(mode)
            .bind(&allowed)
            .bind(&denied)
            .bind(expected as i64)
            .execute(&self.pool)
            .await?
            .rows_affected(),
        };
        if affected == 0 {
            let current = self.policy(principal_id).await?;
            return Err(AppError::Conflict(format!(
                "policy revision conflict: expected r{}, current r{}",
                expected_revision.unwrap_or(0),
                current.revision
            )));
        }
        self.policy(principal_id).await
    }

    async fn append_audit(&self, event: AuditEvent) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO audit_events \
                (id, principal_id, ts_ms, session_id, turn_id, kind, target, decision, \
                 policy_revision, metadata) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(&event.id)
        .bind(&event.principal_id)
        .bind(event.ts_ms as i64)
        .bind(event.session_id.as_deref())
        .bind(event.turn_id.as_deref())
        .bind(event.kind.as_str())
        .bind(&event.target)
        .bind(event.decision)
        .bind(event.policy_revision.map(|r| r as i64))
        .bind(event.metadata.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_audit(
        &self,
        principal_id: &str,
        limit: usize,
        kind: Option<AuditKind>,
    ) -> AppResult<Vec<AuditEvent>> {
        let rows = sqlx::query(
            "SELECT id, principal_id, ts_ms, session_id, turn_id, kind, target, decision, \
                    policy_revision, metadata \
               FROM audit_events \
              WHERE principal_id = $1 AND ($2::text IS NULL OR kind = $2) \
              ORDER BY ts_ms DESC LIMIT $3",
        )
        .bind(principal_id)
        .bind(kind.map(|k| k.as_str().to_string()))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let kind_raw: String = row.try_get("kind")?;
            let kind =
                serde_json::from_value::<AuditKind>(serde_json::Value::String(kind_raw.clone()))
                    .map_err(|_| AppError::BadRequest(format!("stored audit kind: {kind_raw}")))?;
            let metadata: String = row.try_get("metadata")?;
            out.push(AuditEvent {
                id: row.try_get("id")?,
                ts_ms: row.try_get::<i64, _>("ts_ms")?.max(0) as u64,
                principal_id: row.try_get("principal_id")?,
                session_id: row.try_get("session_id")?,
                turn_id: row.try_get("turn_id")?,
                kind,
                target: row.try_get("target")?,
                decision: row.try_get("decision")?,
                policy_revision: row
                    .try_get::<Option<i64>, _>("policy_revision")?
                    .map(|r| r.max(0) as u64),
                metadata: serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null),
            });
        }
        out.reverse();
        Ok(out)
    }

    async fn record_usage(&self, record: UsageRecord) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO usage_events \
                (id, principal_id, sandbox_id, state, started_at_ms, ended_at_ms, \
                 vcpu_millis, memory_mib_millis, disk_mib_millis, input_tokens, output_tokens) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(&record.id)
        .bind(&record.principal_id)
        .bind(&record.sandbox_id)
        .bind(record.state.as_str())
        .bind(record.started_at_ms as i64)
        .bind(record.ended_at_ms as i64)
        .bind(record.resources.vcpu_millis as i64)
        .bind(record.resources.memory_mib_millis as i64)
        .bind(record.resources.disk_mib_millis as i64)
        .bind(record.tokens.map(|t| t.input as i64))
        .bind(record.tokens.map(|t| t.output as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn usage(&self, principal_id: &str) -> AppResult<UsageSummary> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS records, \
                    COALESCE(SUM(vcpu_millis), 0) AS vcpu, \
                    COALESCE(SUM(memory_mib_millis), 0) AS memory, \
                    COALESCE(SUM(disk_mib_millis), 0) AS disk, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens \
               FROM usage_events WHERE principal_id = $1",
        )
        .bind(principal_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(UsageSummary {
            principal_id: principal_id.to_string(),
            records: row.try_get::<i64, _>("records")?.max(0) as u64,
            resources: ResourceUsage {
                vcpu_millis: row.try_get::<i64, _>("vcpu")?.max(0) as u64,
                memory_mib_millis: row.try_get::<i64, _>("memory")?.max(0) as u64,
                disk_mib_millis: row.try_get::<i64, _>("disk")?.max(0) as u64,
            },
            tokens: BilledTokens {
                input: row.try_get::<i64, _>("input_tokens")?.max(0) as u64,
                output: row.try_get::<i64, _>("output_tokens")?.max(0) as u64,
            },
            priced_micros_usd: None,
        })
    }

    async fn put_checkpoint(&self, principal_id: &str, checkpoint: CheckpointRef) -> AppResult<()> {
        checkpoint
            .validate()
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        sqlx::query(
            "INSERT INTO sandbox_checkpoints (id, principal_id, sandbox_id, image, created_at_ms) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&checkpoint.id)
        .bind(principal_id)
        .bind(&checkpoint.source)
        .bind(&checkpoint.image)
        .bind(checkpoint.created_at_ms as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_checkpoints(&self, principal_id: &str) -> AppResult<Vec<CheckpointRef>> {
        let rows = sqlx::query(
            "SELECT id, sandbox_id, image, created_at_ms FROM sandbox_checkpoints \
              WHERE principal_id = $1 ORDER BY created_at_ms DESC",
        )
        .bind(principal_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(CheckpointRef {
                    id: row.try_get("id")?,
                    image: row.try_get("image")?,
                    source: row.try_get("sandbox_id")?,
                    created_at_ms: row.try_get::<i64, _>("created_at_ms")?.max(0) as u64,
                })
            })
            .collect()
    }

    async fn get_checkpoint(&self, principal_id: &str, id: &str) -> AppResult<CheckpointRef> {
        let row = sqlx::query(
            "SELECT id, sandbox_id, image, created_at_ms FROM sandbox_checkpoints \
              WHERE principal_id = $1 AND id = $2",
        )
        .bind(principal_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let row = row.ok_or(AppError::NotFound)?;
        Ok(CheckpointRef {
            id: row.try_get("id")?,
            image: row.try_get("image")?,
            source: row.try_get("sandbox_id")?,
            created_at_ms: row.try_get::<i64, _>("created_at_ms")?.max(0) as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_hosts_round_trip_through_json() {
        let hosts = vec![
            HostPattern::parse("a.example.com").unwrap(),
            HostPattern::parse("*.pypi.org").unwrap(),
        ];
        let json = hosts_to_json(&hosts);
        assert_eq!(hosts_from_json(&json).unwrap(), hosts);
        assert!(hosts_from_json("{").is_err());
        assert!(hosts_from_json("[\"https://x\"]").is_err());
    }
}
