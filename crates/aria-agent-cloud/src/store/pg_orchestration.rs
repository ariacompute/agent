//! Postgres implementations of the orchestration seams
//! ([`TeamStore`] + [`ScheduleSeam`]) and of the session-title projection
//! ([`TitleStore`]).
//!
//! Every table carries `principal_id` and every statement filters on it, so a
//! tenant can neither read nor mutate another tenant's team/job/title — a
//! conflict on someone else's id reports `Denied` instead of silently
//! overwriting.
//!
//! Excluded from the coverage gate (needs a live database); the same semantics
//! are covered by the in-memory providers in the API tests.

use agent_core::seams::{ScheduleEntry, ScheduleSeam, SeamError, SeamResult};
use aria_agent_orchestrator::teams::{AgentTeam, TeamStore, TeamTask};
use async_trait::async_trait;
use sqlx::Row;

use crate::auth::Principal;
use crate::store::{PgStore, TitleRow, TitleStore};
use crate::AppError;

fn io(what: &str, e: impl std::fmt::Display) -> SeamError {
    SeamError::Io(format!("{what}: {e}"))
}

fn tasks_from_json(raw: &str, team: &str) -> SeamResult<Vec<TeamTask>> {
    serde_json::from_str(raw)
        .map_err(|e| SeamError::Io(format!("team {team}: bad tasks JSON: {e}")))
}

fn tasks_to_json(tasks: &[TeamTask]) -> SeamResult<String> {
    serde_json::to_string(tasks)
        .map_err(|e| SeamError::Io(format!("team tasks are not serialisable: {e}")))
}

#[async_trait]
impl TeamStore for PgStore {
    async fn create(&self, mut team: AgentTeam) -> SeamResult<AgentTeam> {
        if team.id.trim().is_empty() {
            team.id = format!("team-{}", uuid::Uuid::new_v4());
        }
        let tasks = tasks_to_json(&team.tasks)?;
        sqlx::query(
            "INSERT INTO agent_teams (id, principal_id, name, tasks) VALUES ($1, $2, $3, $4)",
        )
        .bind(&team.id)
        .bind(&team.owner)
        .bind(&team.name)
        .bind(&tasks)
        .execute(self.pool())
        .await
        .map_err(|e| io("insert team", e))?;
        Ok(team)
    }

    async fn get(&self, owner: &str, id: &str) -> SeamResult<AgentTeam> {
        let row = sqlx::query(
            "SELECT id, name, tasks FROM agent_teams WHERE id = $1 AND principal_id = $2",
        )
        .bind(id)
        .bind(owner)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| io("select team", e))?
        .ok_or_else(|| SeamError::NotFound(format!("team {id}")))?;
        let name: String = row.get("name");
        let raw: String = row.get("tasks");
        Ok(AgentTeam {
            id: row.get("id"),
            owner: owner.to_string(),
            name,
            tasks: tasks_from_json(&raw, id)?,
        })
    }

    async fn list(&self, owner: &str) -> SeamResult<Vec<AgentTeam>> {
        let rows = sqlx::query(
            "SELECT id, name, tasks FROM agent_teams WHERE principal_id = $1 ORDER BY created_at DESC",
        )
        .bind(owner)
        .fetch_all(self.pool())
        .await
        .map_err(|e| io("list teams", e))?;
        rows.into_iter()
            .map(|row| {
                let id: String = row.get("id");
                let raw: String = row.get("tasks");
                Ok(AgentTeam {
                    tasks: tasks_from_json(&raw, &id)?,
                    id,
                    owner: owner.to_string(),
                    name: row.get("name"),
                })
            })
            .collect()
    }

    async fn update(&self, team: AgentTeam) -> SeamResult<AgentTeam> {
        let tasks = tasks_to_json(&team.tasks)?;
        let updated = sqlx::query(
            "UPDATE agent_teams SET name = $3, tasks = $4, updated_at = now() \
             WHERE id = $1 AND principal_id = $2",
        )
        .bind(&team.id)
        .bind(&team.owner)
        .bind(&team.name)
        .bind(&tasks)
        .execute(self.pool())
        .await
        .map_err(|e| io("update team", e))?
        .rows_affected();
        if updated == 0 {
            // Either unknown or owned by another tenant; the distinction leaks
            // nothing, both are "not yours to update".
            return Err(SeamError::NotFound(format!("team {}", team.id)));
        }
        Ok(team)
    }
}

fn row_to_schedule(row: &sqlx::postgres::PgRow) -> ScheduleEntry {
    let payload: String = row.get("payload");
    ScheduleEntry {
        id: row.get("id"),
        owner: row.get("principal_id"),
        name: row.get("name"),
        every_secs: row.get::<Option<i64>, _>("every_secs").map(|v| v as u64),
        at_ms: row.get("at_ms"),
        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
        enabled: row.get("enabled"),
        next_run_ms: row.get("next_run_ms"),
    }
}

const SCHEDULE_COLUMNS: &str =
    "id, principal_id, name, every_secs, at_ms, payload, enabled, next_run_ms";

#[async_trait]
impl ScheduleSeam for PgStore {
    async fn upsert(&self, mut entry: ScheduleEntry) -> SeamResult<ScheduleEntry> {
        entry.validate()?;
        if entry.id.trim().is_empty() {
            entry.id = format!("job-{}", uuid::Uuid::new_v4());
        }
        entry.next_run_ms = match (entry.every_secs, entry.at_ms) {
            (Some(_), None) => entry.next_after(aria_agent_orchestrator::now_ms()),
            (None, Some(at)) => Some(at),
            _ => None,
        };
        if !entry.enabled {
            entry.next_run_ms = None;
        }
        let payload = serde_json::to_string(&entry.payload)
            .map_err(|e| io("schedule payload is not serialisable", e))?;
        let row = sqlx::query(&format!(
            "INSERT INTO scheduled_jobs ({SCHEDULE_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, every_secs = EXCLUDED.every_secs, \
                 at_ms = EXCLUDED.at_ms, payload = EXCLUDED.payload, enabled = EXCLUDED.enabled, \
                 next_run_ms = EXCLUDED.next_run_ms, updated_at = now() \
             WHERE scheduled_jobs.principal_id = EXCLUDED.principal_id \
             RETURNING {SCHEDULE_COLUMNS}"
        ))
        .bind(&entry.id)
        .bind(&entry.owner)
        .bind(&entry.name)
        .bind(entry.every_secs.map(|v| v as i64))
        .bind(entry.at_ms)
        .bind(&payload)
        .bind(entry.enabled)
        .bind(entry.next_run_ms)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| io("upsert schedule", e))?;
        match row {
            Some(row) => Ok(row_to_schedule(&row)),
            None => Err(SeamError::Denied(format!(
                "schedule `{}` belongs to another tenant",
                entry.id
            ))),
        }
    }

    async fn list(&self, owner: &str) -> SeamResult<Vec<ScheduleEntry>> {
        let rows = sqlx::query(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM scheduled_jobs WHERE principal_id = $1 \
             ORDER BY created_at DESC"
        ))
        .bind(owner)
        .fetch_all(self.pool())
        .await
        .map_err(|e| io("list schedules", e))?;
        Ok(rows.iter().map(row_to_schedule).collect())
    }

    async fn remove(&self, owner: &str, id: &str) -> SeamResult<bool> {
        let owner_of: Option<String> =
            sqlx::query("SELECT principal_id FROM scheduled_jobs WHERE id = $1")
                .bind(id)
                .fetch_optional(self.pool())
                .await
                .map_err(|e| io("select schedule", e))?
                .map(|row| row.get("principal_id"));
        match owner_of {
            None => Ok(false),
            Some(found) if found != owner => Err(SeamError::Denied(format!(
                "schedule `{id}` belongs to another tenant"
            ))),
            Some(_) => {
                sqlx::query("DELETE FROM scheduled_jobs WHERE id = $1 AND principal_id = $2")
                    .bind(id)
                    .bind(owner)
                    .execute(self.pool())
                    .await
                    .map_err(|e| io("delete schedule", e))?;
                Ok(true)
            }
        }
    }
}

#[async_trait]
impl TitleStore for PgStore {
    async fn put_title(&self, principal: &Principal, row: &TitleRow) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO session_titles (principal_id, session_id, title, source, updated_at) \
             VALUES ($1, $2, $3, $4, to_timestamp($5::double precision / 1000)) \
             ON CONFLICT (principal_id, session_id) DO UPDATE \
             SET title = EXCLUDED.title, source = EXCLUDED.source, \
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&principal.id)
        .bind(&row.session_id)
        .bind(&row.title)
        .bind(&row.source)
        .bind(row.updated_at)
        .execute(self.pool())
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn get_title(
        &self,
        principal: &Principal,
        session: &str,
    ) -> Result<Option<TitleRow>, AppError> {
        let row = sqlx::query(
            "SELECT session_id, title, source, \
                 (EXTRACT(EPOCH FROM updated_at) * 1000)::BIGINT AS updated_at \
             FROM session_titles WHERE principal_id = $1 AND session_id = $2",
        )
        .bind(&principal.id)
        .bind(session)
        .fetch_optional(self.pool())
        .await
        .map_err(AppError::Db)?;
        Ok(row.map(|r| TitleRow {
            session_id: r.get("session_id"),
            title: r.get("title"),
            source: r.get("source"),
            updated_at: r.get("updated_at"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_json_round_trips() {
        let mut team = AgentTeam::new("t", "n", vec![TeamTask::new("t1", "spec")]).expect("team");
        team.claim("t1", "w").expect("claim");
        let tasks = team.tasks;
        let raw = tasks_to_json(&tasks).unwrap();
        let back = tasks_from_json(&raw, "team-1").unwrap();
        assert_eq!(back, tasks);
        assert!(tasks_from_json("{not json", "team-1").is_err());
    }

    use crate::store::PgStore;

    /// One store over the shared fixture, or an early return when this
    /// environment has no database.
    async fn store() -> Option<PgStore> {
        crate::pg_testkit::maybe_pool().await.map(PgStore::new)
    }

    fn owner() -> String {
        format!("owner-{}", uuid::Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn teams_round_trip_and_refuse_a_foreign_update() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let owner = owner();
        let team = AgentTeam::new(
            &owner,
            "incident",
            vec![TeamTask::new("triage", "read the alert")],
        )
        .expect("team");
        let created = store.create(team).await.expect("create");
        // An empty id is minted by the store, never by the caller.
        assert!(created.id.starts_with("team-"));

        let fetched = store.get(&owner, &created.id).await.expect("get");
        assert_eq!(fetched.name, "incident");
        assert_eq!(fetched.tasks.len(), 1);
        assert_eq!(fetched.owner, owner);

        let mut renamed = fetched.clone();
        renamed.name = "incident v2".into();
        let updated = store.update(renamed.clone()).await.expect("update");
        assert_eq!(updated.name, "incident v2");
        assert_eq!(
            TeamStore::list(&store, &owner).await.expect("list").len(),
            1
        );

        // Unknown and foreign ids are one answer: not yours.
        assert!(matches!(
            store.get(&owner, "ghost").await,
            Err(SeamError::NotFound(_))
        ));
        let ghost = AgentTeam {
            id: "ghost".into(),
            ..renamed.clone()
        };
        assert!(matches!(
            store.update(ghost).await,
            Err(SeamError::NotFound(_))
        ));
        let foreign = store.update(AgentTeam {
            owner: "someone-else".into(),
            ..created
        });
        assert!(matches!(foreign.await, Err(SeamError::NotFound(_))));

        // A duplicate id is a storage error, not a silent overwrite.
        let dup = AgentTeam::new(&owner, "dup", vec![TeamTask::new("t", "x")]).expect("team");
        let dup = store.create(dup).await.expect("first");
        assert!(store.create(dup).await.is_err());
    }

    /// A row written by an older schema (tasks is free text) must surface as a
    /// seam error instead of panicking.
    #[tokio::test]
    async fn teams_report_unreadable_task_json() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let owner = owner();
        let id = format!("team-{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO agent_teams (id, principal_id, name, tasks) VALUES ($1, $2, $3, '{oops')",
        )
        .bind(&id)
        .bind(&owner)
        .bind("broken")
        .execute(store.pool())
        .await
        .expect("insert raw row");

        assert!(matches!(
            store.get(&owner, &id).await,
            Err(SeamError::Io(_))
        ));
        assert!(matches!(
            TeamStore::list(&store, &owner).await,
            Err(SeamError::Io(_))
        ));
    }

    #[tokio::test]
    async fn schedules_compute_the_next_run_and_respect_ownership() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let owner = owner();

        let interval = store
            .upsert(ScheduleEntry::every(&owner, "poll", 30))
            .await
            .expect("upsert interval");
        assert!(interval.id.starts_with("job-"));
        assert!(interval.next_run_ms.is_some());

        let at = aria_agent_orchestrator::now_ms() + 60_000;
        let once = store
            .upsert(ScheduleEntry::at(&owner, "remind", at))
            .await
            .expect("upsert one-shot");
        assert_eq!(once.next_run_ms, Some(at));

        let mut paused = ScheduleEntry::every(&owner, "paused", 60);
        paused.enabled = false;
        let paused = store.upsert(paused).await.expect("upsert paused");
        assert_eq!(paused.next_run_ms, None, "a disabled job has no cursor");

        assert_eq!(
            ScheduleSeam::list(&store, &owner)
                .await
                .expect("list")
                .len(),
            3
        );

        // Both triggers at once is rejected before any SQL runs.
        let mut both = ScheduleEntry::every(&owner, "bad", 10);
        both.at_ms = Some(at);
        assert!(matches!(
            store.upsert(both).await,
            Err(SeamError::Invalid(_))
        ));

        // Removing someone else's job is denied; removing an unknown id is not
        // an error, just `false`.
        let other = owner.clone() + "-other";
        let foreign = store
            .upsert(ScheduleEntry::every(&other, "theirs", 5))
            .await
            .expect("foreign job");
        assert!(matches!(
            store.remove(&owner, &foreign.id).await,
            Err(SeamError::Denied(_))
        ));
        assert!(!store.remove(&owner, "ghost").await.expect("remove ghost"));

        // Re-using a foreign id must not hijack the row (ON CONFLICT ... WHERE).
        let mut hijack = ScheduleEntry::every(&owner, "mine now", 5);
        hijack.id = foreign.id.clone();
        assert!(matches!(
            store.upsert(hijack).await,
            Err(SeamError::Denied(_))
        ));

        assert!(store.remove(&owner, &interval.id).await.expect("remove"));
        assert_eq!(
            ScheduleSeam::list(&store, &owner)
                .await
                .expect("list")
                .len(),
            2
        );
    }

    /// A payload that is not JSON decodes as `null` instead of failing the read.
    #[tokio::test]
    async fn schedules_decode_a_broken_payload_as_null() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let owner = owner();
        let job = store
            .upsert(ScheduleEntry::every(&owner, "poll", 30))
            .await
            .expect("upsert");
        sqlx::query("UPDATE scheduled_jobs SET payload = '{oops' WHERE id = $1")
            .bind(&job.id)
            .execute(store.pool())
            .await
            .expect("corrupt payload");
        let rows = ScheduleSeam::list(&store, &owner).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payload, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn titles_are_upserted_per_tenant() {
        let store = crate::pg_testkit::db_or_skip!(store().await);
        let principal = crate::auth::Principal {
            id: owner(),
            kind: crate::auth::PrincipalKind::User,
            scopes: crate::auth::ApiKeyScopes::read_write(),
        };
        let session = format!("sess-{}", uuid::Uuid::new_v4().simple());
        let row = TitleRow {
            session_id: session.clone(),
            title: "first prompt".into(),
            source: "first_prompt".into(),
            updated_at: 1_700_000_000_000,
        };
        store.put_title(&principal, &row).await.expect("put");

        let stored = store
            .get_title(&principal, &session)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(stored.title, "first prompt");
        assert_eq!(stored.source, "first_prompt");
        assert_eq!(stored.session_id, session);

        // A provider override replaces the deterministic title in place.
        let overridden = TitleRow {
            title: "provider title".into(),
            source: "provider".into(),
            updated_at: 1_700_000_001_000,
            ..row
        };
        store
            .put_title(&principal, &overridden)
            .await
            .expect("overwrite");
        let stored = store
            .get_title(&principal, &session)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(stored.source, "provider");

        assert!(store
            .get_title(&principal, "ghost")
            .await
            .expect("get ghost")
            .is_none());
    }
}
