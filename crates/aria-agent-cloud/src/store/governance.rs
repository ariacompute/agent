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
        // `SUM(bigint)` is `NUMERIC` in Postgres, and a NUMERIC cannot be
        // decoded into `i64`; cast every aggregate back to BIGINT. Without the
        // casts this query fails on the very first row it sees.
        let row = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS records, \
                    COALESCE(SUM(vcpu_millis), 0)::BIGINT AS vcpu, \
                    COALESCE(SUM(memory_mib_millis), 0)::BIGINT AS memory, \
                    COALESCE(SUM(disk_mib_millis), 0)::BIGINT AS disk, \
                    COALESCE(SUM(input_tokens), 0)::BIGINT AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0)::BIGINT AS output_tokens \
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
    use agent_core::seams::MeterState;

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

    /// DB-gated (see `crate::pg_testkit`): the store maps every row back into
    /// the seam types, so the round trip is what a deployment actually runs.
    async fn store() -> Option<PgGovernanceStore> {
        crate::pg_testkit::maybe_pool()
            .await
            .map(PgGovernanceStore::new)
    }

    fn tenant() -> String {
        format!("gov-{}", uuid::Uuid::new_v4().simple())
    }

    fn policy(mode: EgressMode, allow: &[&str]) -> EgressPolicy {
        EgressPolicy::new(
            0,
            mode,
            allow
                .iter()
                .map(|h| HostPattern::parse(h).unwrap())
                .collect(),
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn policies_are_revision_guarded() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let tenant = tenant();

        // No row yet: deny-by-default, revision 1.
        let initial = store.policy(&tenant).await.expect("policy");
        assert_eq!(initial, EgressPolicy::deny_all());

        let created = store
            .replace_policy(
                &tenant,
                None,
                policy(EgressMode::Allowlist, &["api.openai.com", "*.pypi.org"]),
            )
            .await
            .expect("create");
        assert_eq!(created.revision, 1);
        assert_eq!(created.allowed.len(), 2);

        // Creating again without a revision is a no-op, not a silent overwrite.
        assert!(matches!(
            store
                .replace_policy(&tenant, None, policy(EgressMode::DenyAll, &[]))
                .await,
            Err(AppError::Conflict(_))
        ));

        let bumped = store
            .replace_policy(&tenant, Some(1), policy(EgressMode::Denylist, &[]))
            .await
            .expect("bump");
        assert_eq!(bumped.revision, 2);
        assert_eq!(bumped.mode, EgressMode::Denylist);

        // A stale writer is refused and told what the current revision is.
        match store
            .replace_policy(&tenant, Some(1), policy(EgressMode::DenyAll, &[]))
            .await
        {
            Err(AppError::Conflict(message)) => assert!(message.contains("current r2")),
            other => panic!("expected a conflict, got {other:?}"),
        }

        // Validation runs before any SQL.
        assert!(matches!(
            store
                .replace_policy(&tenant, Some(2), policy(EgressMode::Allowlist, &[]))
                .await,
            Err(AppError::BadRequest(_))
        ));
    }

    /// Rows written by other means (an operator's psql, an older revision) must
    /// fail loudly instead of producing a half-parsed policy.
    #[tokio::test]
    async fn a_corrupt_policy_row_is_a_bad_request() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let pool = store.pool.clone();
        let tenant = tenant();
        sqlx::query(
            "INSERT INTO egress_policies (principal_id, revision, mode, allowed, denied) \
             VALUES ($1, 1, 'bogus', '[]', '[]')",
        )
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("insert");
        assert!(matches!(
            store.policy(&tenant).await,
            Err(AppError::BadRequest(_))
        ));

        sqlx::query("UPDATE egress_policies SET mode = 'deny_all', allowed = '{oops' WHERE principal_id = $1")
            .bind(&tenant)
            .execute(&pool)
            .await
            .expect("corrupt allowed");
        assert!(matches!(
            store.policy(&tenant).await,
            Err(AppError::BadRequest(_))
        ));

        sqlx::query(
            "UPDATE egress_policies SET allowed = '[\"https://x\"]' WHERE principal_id = $1",
        )
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("corrupt host");
        assert!(matches!(
            store.policy(&tenant).await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn audit_events_round_trip_newest_first_and_filter_by_kind() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let tenant = tenant();
        let base = AuditEvent {
            id: String::new(),
            ts_ms: 1_000,
            principal_id: tenant.clone(),
            session_id: Some("sess".into()),
            turn_id: None,
            kind: AuditKind::EgressDecision,
            target: "api.openai.com".into(),
            decision: Some(true),
            policy_revision: Some(3),
            metadata: serde_json::json!({ "host": "api.openai.com" }),
        };
        let first_id = format!("a1-{tenant}");
        let second_id = format!("a2-{tenant}");
        let mut first = base.clone();
        first.id = first_id.clone();
        store.append_audit(first).await.expect("append first");

        let mut second = base.clone();
        second.id = second_id.clone();
        second.ts_ms = 2_000;
        second.kind = AuditKind::ToolExecuted;
        second.decision = None;
        second.session_id = None;
        second.policy_revision = None;
        store.append_audit(second).await.expect("append second");

        let all = store.list_audit(&tenant, 10, None).await.expect("list");
        assert_eq!(all.len(), 2);
        // Newest first once the store reverses the DB's DESC order.
        assert_eq!(all[0].id, first_id);
        assert_eq!(all[0].metadata["host"], "api.openai.com");

        let only_tools = store
            .list_audit(&tenant, 10, Some(AuditKind::ToolExecuted))
            .await
            .expect("filtered");
        assert_eq!(only_tools.len(), 1);
        assert_eq!(only_tools[0].id, second_id);

        // An unknown kind in the table is a decode failure, not a guess.
        let pool = store.pool.clone();
        sqlx::query("UPDATE audit_events SET kind = 'not_a_kind' WHERE id = $1")
            .bind(&second_id)
            .execute(&pool)
            .await
            .expect("corrupt kind");
        assert!(matches!(
            store.list_audit(&tenant, 10, None).await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn usage_aggregates_per_principal() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let tenant = tenant();
        let empty = store.usage(&tenant).await.expect("empty usage");
        assert_eq!(empty.records, 0);
        assert!(empty.resources.is_zero());
        assert_eq!(empty.tokens, BilledTokens::default());

        let mut record = UsageRecord::new(
            tenant.clone(),
            "sandbox-1",
            MeterState::Running,
            1_000,
            2_000,
            ResourceUsage {
                vcpu_millis: 10,
                memory_mib_millis: 20,
                disk_mib_millis: 30,
            },
        );
        record.tokens = Some(BilledTokens::new(7, 3));
        store.record_usage(record).await.expect("record");

        let mut bare = UsageRecord::new(
            tenant.clone(),
            "sandbox-2",
            MeterState::Destroyed,
            2_000,
            3_000,
            ResourceUsage::default(),
        );
        bare.tokens = None;
        store
            .record_usage(bare)
            .await
            .expect("record without tokens");

        let summary = store.usage(&tenant).await.expect("usage");
        assert_eq!(summary.records, 2);
        assert_eq!(summary.resources.vcpu_millis, 10);
        assert_eq!(summary.tokens, BilledTokens::new(7, 3));
        assert_eq!(summary.principal_id, tenant);
    }

    #[tokio::test]
    async fn checkpoints_are_tenant_scoped_and_validated() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let owner = tenant();
        let checkpoint = CheckpointRef::new("img:1", "sandbox-1", 1_000);
        store
            .put_checkpoint(&owner, checkpoint.clone())
            .await
            .expect("put");

        let listed = store.list_checkpoints(&owner).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], checkpoint);
        assert_eq!(
            store
                .get_checkpoint(&owner, &checkpoint.id)
                .await
                .expect("get"),
            checkpoint
        );
        assert!(matches!(
            store.get_checkpoint(&owner, "ghost").await,
            Err(AppError::NotFound)
        ));
        // Another tenant sees nothing.
        assert!(store
            .list_checkpoints(&tenant())
            .await
            .expect("foreign list")
            .is_empty());

        // A blank image never reaches Postgres.
        let broken = CheckpointRef {
            image: "  ".into(),
            ..checkpoint
        };
        assert!(matches!(
            store.put_checkpoint(&owner, broken).await,
            Err(AppError::BadRequest(_))
        ));
    }
}
