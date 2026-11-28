//! Postgres provider for the A2A task port.
//!
//! Every statement carries the tenant filter in the `WHERE` clause (`AND ($3 OR
//! principal_id = $2)`), so an admin reads across tenants while a user can only
//! see their own rows — and an invisible row answers `NotFound`, never
//! "forbidden", so ids cannot be probed.
//!
//! `transition` is a real compare-and-set: it reads the row, rejects an illegal
//! move, then updates `WHERE state = <observed>`. A concurrent cancel that wins
//! the race turns this call into `Ok(None)` instead of an overwrite.
//!
//! Excluded from the coverage gate like the rest of `store/pg*`: these paths need
//! a live database. The module's own DB-gated tests are the direct check of the
//! SQL; [`super::store::MemoryA2aTasks`] pins the same semantics offline.

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};

use aria_agent_a2a::types::{Artifact, PushNotificationConfig};
use aria_agent_a2a::TaskState;

use crate::auth::{Principal, PrincipalKind};
use crate::store::can_see;
use crate::AppError;

use super::store::{A2aEventRecord, A2aTaskRow, A2aTaskStore};

fn encode<T: Serialize>(value: &T, what: &str) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|e| AppError::BadRequest(format!("{what}: {e}")))
}

fn decode<T: DeserializeOwned>(raw: &str, what: &str) -> Result<T, AppError> {
    serde_json::from_str(raw).map_err(|e| AppError::BadRequest(format!("{what}: {e}")))
}

const COLS: &str = "task_id, principal_id, agent_id, context_id, turn_id, state, \
                    status_message, artifacts, push_config, push_last, \
                    created_at_ms, updated_at_ms";

fn row_from(row: &sqlx::postgres::PgRow) -> Result<A2aTaskRow, AppError> {
    let state_raw: String = row.get("state");
    let state = TaskState::parse(&state_raw)
        .ok_or_else(|| AppError::BadRequest(format!("stored A2A state {state_raw}")))?;
    let artifacts: Vec<Artifact> = match row.get::<Option<String>, _>("artifacts") {
        Some(raw) => decode(&raw, "artifacts")?,
        None => Vec::new(),
    };
    let status = row
        .get::<Option<String>, _>("status_message")
        .map(|raw| decode::<Value>(&raw, "status_message"))
        .transpose()?;
    let push = row
        .get::<Option<String>, _>("push_config")
        .map(|raw| decode::<PushNotificationConfig>(&raw, "push_config"))
        .transpose()?;
    let push_last = row
        .get::<Option<String>, _>("push_last")
        .map(|raw| decode::<Value>(&raw, "push_last"))
        .transpose()?;
    Ok(A2aTaskRow {
        task_id: row.get("task_id"),
        principal_id: row.get("principal_id"),
        agent_id: row.get("agent_id"),
        context_id: row.get("context_id"),
        turn_id: row.get("turn_id"),
        state,
        status,
        artifacts,
        push,
        push_last,
        created_at_ms: row.get("created_at_ms"),
        updated_at_ms: row.get("updated_at_ms"),
    })
}

/// Postgres-backed [`A2aTaskStore`].
pub struct PgA2aTasks {
    pool: PgPool,
}

impl PgA2aTasks {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn read(&self, principal: &Principal, task_id: &str) -> Result<A2aTaskRow, AppError> {
        let admin = principal.kind == PrincipalKind::Admin;
        let row = sqlx::query(&format!(
            "SELECT {COLS} FROM a2a_tasks WHERE task_id = $1 AND ($3 OR principal_id = $2)"
        ))
        .bind(task_id)
        .bind(&principal.id)
        .bind(admin)
        .fetch_optional(&self.pool)
        .await
        .map_err(AppError::Db)?;
        match row {
            Some(row) => {
                let row = row_from(&row)?;
                // Defence in depth: the WHERE clause already enforced this.
                if !can_see(principal, &row.principal_id) {
                    return Err(AppError::NotFound);
                }
                Ok(row)
            }
            None => Err(AppError::NotFound),
        }
    }
}

#[async_trait]
impl A2aTaskStore for PgA2aTasks {
    async fn create(&self, principal: &Principal, row: &A2aTaskRow) -> Result<(), AppError> {
        if !can_see(principal, &row.principal_id) {
            return Err(AppError::NotFound);
        }
        let status = match &row.status {
            Some(status) => Some(encode(status, "status_message")?),
            None => None,
        };
        sqlx::query(
            "INSERT INTO a2a_tasks \
             (task_id, principal_id, agent_id, context_id, turn_id, state, \
              status_message, artifacts, push_config, push_last, \
              created_at_ms, updated_at_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        )
        .bind(&row.task_id)
        .bind(&row.principal_id)
        .bind(&row.agent_id)
        .bind(&row.context_id)
        .bind(&row.turn_id)
        .bind(row.state.as_str())
        .bind(status)
        .bind(encode(&row.artifacts, "artifacts")?)
        .bind(
            row.push
                .as_ref()
                .map(|cfg| encode(cfg, "push_config"))
                .transpose()?,
        )
        .bind(
            row.push_last
                .as_ref()
                .map(|value| encode(value, "push_last"))
                .transpose()?,
        )
        .bind(row.created_at_ms)
        .bind(row.updated_at_ms)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn get(&self, principal: &Principal, task_id: &str) -> Result<A2aTaskRow, AppError> {
        self.read(principal, task_id).await
    }

    async fn transition(
        &self,
        principal: &Principal,
        task_id: &str,
        next: TaskState,
        status: Option<Value>,
        now_ms: i64,
    ) -> Result<Option<A2aTaskRow>, AppError> {
        let current = self.read(principal, task_id).await?;
        if current.state.is_terminal() {
            return Ok(None);
        }
        if !current.state.can_transition(next) {
            return Err(AppError::Conflict(format!(
                "task {task_id} cannot move from {} to {}",
                current.state,
                next.as_str()
            )));
        }
        let status = match status {
            Some(status) => Some(encode(&status, "status_message")?),
            None => current
                .status
                .as_ref()
                .map(|status| encode(status, "status_message"))
                .transpose()?,
        };
        let updated = sqlx::query(&format!(
            "UPDATE a2a_tasks SET state = $4, status_message = $5, updated_at_ms = $6 \
             WHERE task_id = $1 AND principal_id = $2 AND state = $3 \
             RETURNING {COLS}"
        ))
        .bind(task_id)
        .bind(&current.principal_id)
        .bind(current.state.as_str())
        .bind(next.as_str())
        .bind(status)
        .bind(now_ms)
        .fetch_optional(&self.pool)
        .await
        .map_err(AppError::Db)?;
        match updated {
            Some(row) => Ok(Some(row_from(&row)?)),
            None => {
                // Either another writer moved it, or it just became terminal.
                let latest = self.read(principal, task_id).await?;
                if latest.state.is_terminal() {
                    Ok(None)
                } else {
                    Err(AppError::Conflict(format!(
                        "task {task_id} changed while transitioning to {}",
                        next.as_str()
                    )))
                }
            }
        }
    }

    async fn add_artifact(
        &self,
        principal: &Principal,
        task_id: &str,
        artifact: &Artifact,
        now_ms: i64,
    ) -> Result<Option<A2aTaskRow>, AppError> {
        let current = self.read(principal, task_id).await?;
        if current.state.is_terminal() {
            return Ok(None);
        }
        let mut artifacts = current.artifacts.clone();
        artifacts.push(artifact.clone());
        let updated = sqlx::query(&format!(
            "UPDATE a2a_tasks SET artifacts = $3, updated_at_ms = $4 \
             WHERE task_id = $1 AND principal_id = $2 AND state = $5 \
             RETURNING {COLS}"
        ))
        .bind(task_id)
        .bind(&current.principal_id)
        .bind(encode(&artifacts, "artifacts")?)
        .bind(now_ms)
        .bind(current.state.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(AppError::Db)?;
        match updated {
            Some(row) => Ok(Some(row_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn append_event(
        &self,
        principal: &Principal,
        task_id: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<u64, AppError> {
        // Visibility first: an invisible task must not get an event even if the
        // caller guessed the id.
        self.read(principal, task_id).await?;
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO a2a_task_events (task_id, seq, kind, payload) \
             SELECT $1, COALESCE(MAX(seq), 0) + 1, $2, $3 FROM a2a_task_events \
             WHERE task_id = $1 RETURNING seq",
        )
        .bind(task_id)
        .bind(kind)
        .bind(encode(payload, "event payload")?)
        .fetch_one(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(seq as u64)
    }

    async fn events_since(
        &self,
        principal: &Principal,
        task_id: &str,
        since: u64,
        limit: usize,
    ) -> Result<Vec<A2aEventRecord>, AppError> {
        self.read(principal, task_id).await?;
        let rows = sqlx::query(
            "SELECT seq, kind, payload FROM a2a_task_events \
             WHERE task_id = $1 AND seq > $2 ORDER BY seq ASC LIMIT $3",
        )
        .bind(task_id)
        .bind(since as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(AppError::Db)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(A2aEventRecord {
                seq: row.get::<i64, _>("seq") as u64,
                kind: row.get("kind"),
                payload: decode(row.get("payload"), "event payload")?,
            });
        }
        Ok(events)
    }

    async fn set_push(
        &self,
        principal: &Principal,
        task_id: &str,
        config: &PushNotificationConfig,
    ) -> Result<(), AppError> {
        let current = self.read(principal, task_id).await?;
        sqlx::query("UPDATE a2a_tasks SET push_config = $2 WHERE task_id = $1")
            .bind(&current.task_id)
            .bind(encode(config, "push_config")?)
            .execute(&self.pool)
            .await
            .map_err(AppError::Db)?;
        Ok(())
    }

    async fn set_push_last(
        &self,
        principal: &Principal,
        task_id: &str,
        result: &Value,
        now_ms: i64,
    ) -> Result<(), AppError> {
        let current = self.read(principal, task_id).await?;
        sqlx::query("UPDATE a2a_tasks SET push_last = $2, updated_at_ms = $3 WHERE task_id = $1")
            .bind(&current.task_id)
            .bind(encode(result, "push_last")?)
            .bind(now_ms)
            .execute(&self.pool)
            .await
            .map_err(AppError::Db)?;
        Ok(())
    }

    async fn fail_orphans(&self, reason: &str, now_ms: i64) -> Result<usize, AppError> {
        let status = serde_json::json!({
            "state": "failed",
            "message": {
                "kind": "message",
                "role": "agent",
                "parts": [{"kind": "text", "type": "text", "text": reason}],
            },
        });
        let result = sqlx::query(
            "UPDATE a2a_tasks SET state = 'failed', status_message = $1, updated_at_ms = $2 \
             WHERE state NOT IN ('completed', 'failed', 'canceled')",
        )
        .bind(encode(&status, "status_message")?)
        .bind(now_ms)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(result.rows_affected() as usize)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::auth::PrincipalKind;
    use crate::pg_testkit::{db_or_skip, maybe_pool};

    /// `fail_orphans` is deliberately global (it closes whatever a dead process
    /// left behind), so every test that touches `a2a_tasks` takes this lock:
    /// one test's live row must not be closed by another test's sweep.
    static A2A_DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn user(id: &str) -> Principal {
        Principal {
            id: id.to_string(),
            kind: PrincipalKind::User,
            scopes: crate::auth::ApiKeyScopes::read_write(),
        }
    }

    fn row(task_id: &str, principal_id: &str) -> A2aTaskRow {
        A2aTaskRow::submitted(
            task_id,
            principal_id,
            "agent-pg",
            "sess-pg",
            "turn-pg",
            json!({"state": "submitted"}),
            1_000,
        )
    }

    async fn cleanup(pool: &PgPool, task_id: &str) {
        sqlx::query("DELETE FROM a2a_task_events WHERE task_id = $1")
            .bind(task_id)
            .execute(pool)
            .await
            .expect("clean events");
        sqlx::query("DELETE FROM a2a_tasks WHERE task_id = $1")
            .bind(task_id)
            .execute(pool)
            .await
            .expect("clean task");
    }

    /// The SQL that the memory provider cannot prove: round-trip of every
    /// column, the CAS transition, the per-task event sequence and tenant
    /// scoping.
    #[tokio::test]
    async fn the_pg_store_round_trips_columns_and_enforces_the_lifecycle() {
        let _guard = A2A_DB.lock().await;
        let pool = db_or_skip!(maybe_pool().await);
        let store = PgA2aTasks::new(pool.clone());
        let admin = Principal::admin();
        let tenant = user("tenant-pg");
        let other = user("tenant-pg-other");
        let task_id = format!("task_pg_{}", Uuid::new_v4().simple());

        // Another tenant can neither write the row nor read it afterwards.
        assert!(matches!(
            store.create(&other, &row(&task_id, &tenant.id)).await,
            Err(AppError::NotFound)
        ));
        store
            .create(&tenant, &row(&task_id, &tenant.id))
            .await
            .expect("create");

        let stored = store.get(&tenant, &task_id).await.expect("read back");
        assert_eq!(stored.principal_id, tenant.id);
        assert_eq!(stored.state, TaskState::Submitted);
        assert_eq!(stored.status, Some(json!({"state": "submitted"})));
        assert!(stored.artifacts.is_empty());
        assert_eq!(stored.created_at_ms, 1_000);
        // Invisible to another tenant, visible to an admin.
        assert!(matches!(
            store.get(&other, &task_id).await,
            Err(AppError::NotFound)
        ));
        assert_eq!(
            store
                .get(&admin, &task_id)
                .await
                .expect("admin read")
                .task_id,
            task_id
        );

        // An illegal move is a conflict, not a silent write.
        assert!(matches!(
            store
                .transition(&tenant, &task_id, TaskState::InputRequired, None, 1_100)
                .await,
            Err(AppError::Conflict(_))
        ));
        let working = store
            .transition(&tenant, &task_id, TaskState::Working, None, 1_200)
            .await
            .expect("transition")
            .expect("live row moves");
        assert_eq!(working.state, TaskState::Working);
        // `None` keeps the previous status message.
        assert_eq!(working.status, Some(json!({"state": "submitted"})));
        let completed = store
            .transition(
                &tenant,
                &task_id,
                TaskState::Completed,
                Some(json!({"state": "completed"})),
                1_300,
            )
            .await
            .expect("transition")
            .expect("live row moves");
        assert_eq!(completed.status, Some(json!({"state": "completed"})));
        assert_eq!(completed.updated_at_ms, 1_300);

        // Terminal rows are frozen: no later transition, no later artifact.
        assert!(store
            .transition(&tenant, &task_id, TaskState::Canceled, None, 1_400)
            .await
            .expect("terminal transition is not an error")
            .is_none());
        let artifact = Artifact::text("final", 0, "hello");
        assert!(store
            .add_artifact(&tenant, &task_id, &artifact, 1_500)
            .await
            .expect("artifact on a terminal task is not an error")
            .is_none());

        // A live task appends artifacts, and the event log numbers per task.
        let live = format!("task_pg_{}", Uuid::new_v4().simple());
        store
            .create(&tenant, &row(&live, &tenant.id))
            .await
            .expect("create live");
        let with_artifact = store
            .add_artifact(&tenant, &live, &artifact, 1_600)
            .await
            .expect("artifact")
            .expect("live row accepts artifacts");
        assert_eq!(with_artifact.artifacts, vec![artifact.clone()]);
        assert_eq!(
            store
                .append_event(&tenant, &live, "task", &json!({"n": 1}))
                .await
                .expect("first event"),
            1
        );
        assert_eq!(
            store
                .append_event(&tenant, &live, "status", &json!({"n": 2}))
                .await
                .expect("second event"),
            2
        );
        let events = store
            .events_since(&tenant, &live, 0, 10)
            .await
            .expect("events");
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].seq, events[1].seq), (1, 2));
        assert_eq!(events[1].kind, "status");
        assert_eq!(events[1].payload, json!({"n": 2}));
        assert_eq!(
            store
                .events_since(&tenant, &live, 1, 10)
                .await
                .expect("since")
                .len(),
            1
        );
        assert_eq!(
            store
                .events_since(&tenant, &live, 0, 1)
                .await
                .expect("limit")
                .len(),
            1
        );
        // An invisible task leaks nothing, not even its event log.
        assert!(matches!(
            store.events_since(&other, &live, 0, 10).await,
            Err(AppError::NotFound)
        ));
        assert!(matches!(
            store
                .append_event(&other, &live, "status", &json!({}))
                .await,
            Err(AppError::NotFound)
        ));

        // Push config and delivery receipt round-trip.
        let config = PushNotificationConfig {
            id: Some("p1".into()),
            url: "https://hook.test/a2a".into(),
            token: Some("tok".into()),
            authentication: None,
        };
        store
            .set_push(&tenant, &live, &config)
            .await
            .expect("set push");
        assert_eq!(
            store.get(&tenant, &live).await.expect("push read").push,
            Some(config.clone())
        );
        store
            .set_push_last(&tenant, &live, &json!({"delivered": true}), 1_700)
            .await
            .expect("set push_last");
        let with_receipt = store.get(&tenant, &live).await.expect("receipt read");
        assert_eq!(with_receipt.push_last, Some(json!({"delivered": true})));
        assert_eq!(with_receipt.updated_at_ms, 1_700);

        cleanup(&pool, &task_id).await;
        cleanup(&pool, &live).await;
    }

    /// `fail_orphans` is the restart guarantee: every non-terminal row is closed
    /// once, with the reason in its status message.
    #[tokio::test]
    async fn fail_orphans_closes_every_live_row_once() {
        let _guard = A2A_DB.lock().await;
        let pool = db_or_skip!(maybe_pool().await);
        let store = PgA2aTasks::new(pool.clone());
        let admin = Principal::admin();
        let task_id = format!("task_pg_{}", Uuid::new_v4().simple());
        store
            .create(&admin, &row(&task_id, &admin.id))
            .await
            .expect("create");

        let closed = store
            .fail_orphans("process restarted; task cannot continue", 2_000)
            .await
            .expect("sweep");
        assert!(closed >= 1, "the live row was not closed");
        let failed = store.get(&admin, &task_id).await.expect("read back");
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(
            failed.status.as_ref().unwrap()["message"]["parts"][0]["text"],
            "process restarted; task cannot continue"
        );
        // Terminal rows are never rewritten, so a second sweep closes nothing.
        assert_eq!(
            store
                .fail_orphans("again", 2_100)
                .await
                .expect("second sweep"),
            0
        );

        cleanup(&pool, &task_id).await;
    }
}
