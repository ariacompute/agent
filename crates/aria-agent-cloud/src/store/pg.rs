//! Postgres implementation of the store traits.
//!
//! This file is **excluded from the coverage gate**: it requires a live
//! Postgres with the `vector` extension. The SQL below is the production
//! contract — table names, column lists and tenant filters must not change
//! without a migration (see `migrations/` and `docs/adr/0010`).
//!
//! DB-gated tests at the bottom run only when `DATABASE_URL` is set; they skip
//! (never fabricate a pass) otherwise.

use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Executor, Row};

use crate::auth::{ApiKeyScopes, Principal, PrincipalKind};
use crate::store::{
    scopes_from_string, scopes_to_string, AgentStore, ApiKeyRow, ApiKeyStore, ItemRow,
    NewApiKeyRow, ResolvedKey, SessionRow, SessionStore, TurnRow,
};
use crate::types::{Agent, AgentReasoning, AgentText, MultiAgentConfig, AGENT_OBJECT};
use crate::AppError;

/// Column list shared by every agent read. `created_at` is returned as epoch
/// millis so no chrono/time dependency is needed.
const AGENT_COLS: &str = "id, name, model, instructions, reasoning_effort, reasoning_summary, \
     multi_agent_enabled, max_concurrent_subagents, service_tier, text_format, text_verbosity, \
     tools, (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at";

const SESSION_COLS: &str = "id, principal_id, agent_id, instructions, status, \
     (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at";

const TURN_COLS: &str = "id, principal_id, session_id, agent_id, status, output, \
     (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at, \
     (EXTRACT(EPOCH FROM completed_at) * 1000)::BIGINT AS completed_at";

const ITEM_COLS: &str = "id, principal_id, session_id, turn_id, item_type, role, status, content, \
     seq, (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at";

/// Marker inside the read queries below that [`scope_sql`] replaces with the
/// per-tenant filter. It always sits **after** the `WHERE` predicates and
/// **before** any `ORDER BY` — appending the filter at the end of the statement
/// would land inside `ORDER BY` and produce invalid SQL.
const SCOPE: &str = "/*scope*/";

/// Postgres-backed stores. One value implements all three traits; it is cheap
/// to clone (the pool is an `Arc` internally).
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn row_to_agent(r: &PgRow) -> Agent {
    let tools: String = r.get::<Option<String>, _>("tools").unwrap_or_default();
    let tools: Vec<serde_json::Value> = serde_json::from_str(&tools).unwrap_or_default();
    let (format, verbosity) = (
        r.get::<Option<String>, _>("text_format"),
        r.get::<Option<String>, _>("text_verbosity"),
    );
    let text = if format.is_none() && verbosity.is_none() {
        None
    } else {
        Some(AgentText { format, verbosity })
    };
    Agent {
        id: r.get("id"),
        object: AGENT_OBJECT.into(),
        name: r.get("name"),
        model: r.get("model"),
        instructions: r.get("instructions"),
        reasoning: AgentReasoning {
            effort: r.get("reasoning_effort"),
            summary: r.get("reasoning_summary"),
        },
        multi_agent: MultiAgentConfig {
            enabled: r.get::<bool, _>("multi_agent_enabled"),
            max_concurrent_subagents: r
                .get::<Option<i32>, _>("max_concurrent_subagents")
                .map(|v| v as u32),
        },
        service_tier: r
            .get::<Option<String>, _>("service_tier")
            .unwrap_or_else(|| "auto".into()),
        text,
        tools,
        created_at: r.get::<i64, _>("created_at"),
    }
}

fn row_to_session(r: &PgRow) -> SessionRow {
    SessionRow {
        id: r.get("id"),
        principal_id: r.get("principal_id"),
        agent_id: r.get("agent_id"),
        instructions: r.get("instructions"),
        status: r.get("status"),
        created_at: r.get("created_at"),
    }
}

fn row_to_turn(r: &PgRow) -> TurnRow {
    TurnRow {
        id: r.get("id"),
        principal_id: r.get("principal_id"),
        session_id: r.get("session_id"),
        agent_id: r.get("agent_id"),
        status: r.get("status"),
        output: r.get("output"),
        created_at: r.get("created_at"),
        completed_at: r.get("completed_at"),
    }
}

fn row_to_item(r: &PgRow) -> ItemRow {
    let raw: String = r.get("content");
    let content = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
    ItemRow {
        id: r.get("id"),
        principal_id: r.get("principal_id"),
        session_id: r.get("session_id"),
        turn_id: r.get("turn_id"),
        kind: r.get("item_type"),
        role: r.get("role"),
        status: r.get("status"),
        content,
        seq: r.get("seq"),
        created_at: r.get("created_at"),
    }
}

/// Substitute the [`SCOPE`] marker with the tenant filter. Admins read across
/// tenants, so the marker is simply removed for them. `bound` is the number of
/// positional parameters already bound by the caller.
fn scope_sql(base: &str, principal: &Principal, bound: usize) -> String {
    if principal.kind == PrincipalKind::Admin {
        base.replace(SCOPE, "")
    } else {
        base.replace(SCOPE, &format!("AND principal_id = ${}", bound + 1))
    }
}

/// Run a tenant-scoped read (`params` are bound in order; the principal id is
/// appended last for non-admins).
async fn tenant_query(
    pool: &PgPool,
    principal: &Principal,
    base: &str,
    params: &[&str],
) -> Result<Vec<PgRow>, AppError> {
    let sql = scope_sql(base, principal, params.len());
    let mut q = sqlx::query(&sql);
    for p in params {
        q = q.bind(p);
    }
    if principal.kind != PrincipalKind::Admin {
        q = q.bind(&principal.id);
    }
    q.fetch_all(pool).await.map_err(AppError::Db)
}

#[async_trait]
impl AgentStore for PgStore {
    async fn insert_agent(&self, principal: &Principal, agent: &Agent) -> Result<(), AppError> {
        let (text_format, text_verbosity) = match &agent.text {
            Some(t) => (t.format.clone(), t.verbosity.clone()),
            None => (None, None),
        };
        let tools = serde_json::to_string(&agent.tools)
            .map_err(|e| AppError::BadRequest(format!("invalid tools: {e}")))?;
        sqlx::query(
            "INSERT INTO agents \
             (id, name, model, instructions, reasoning_effort, reasoning_summary, \
              multi_agent_enabled, max_concurrent_subagents, service_tier, text_format, \
              text_verbosity, tools, principal_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
        )
        .bind(&agent.id)
        .bind(&agent.name)
        .bind(&agent.model)
        .bind(&agent.instructions)
        .bind(&agent.reasoning.effort)
        .bind(&agent.reasoning.summary)
        .bind(agent.multi_agent.enabled)
        .bind(agent.multi_agent.max_concurrent_subagents.map(|v| v as i32))
        .bind(&agent.service_tier)
        .bind(&text_format)
        .bind(&text_verbosity)
        .bind(&tools)
        .bind(&principal.id)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn list_agents(&self, principal: &Principal, limit: i64) -> Result<Vec<Agent>, AppError> {
        let rows = if principal.kind == PrincipalKind::Admin {
            sqlx::query(&format!(
                "SELECT {AGENT_COLS} FROM agents ORDER BY created_at DESC LIMIT $1"
            ))
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(&format!(
                "SELECT {AGENT_COLS} FROM agents WHERE principal_id = $1 \
                 ORDER BY created_at DESC LIMIT $2"
            ))
            .bind(&principal.id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(AppError::Db)?;
        Ok(rows.iter().map(row_to_agent).collect())
    }

    async fn get_agent(&self, principal: &Principal, id_or_name: &str) -> Result<Agent, AppError> {
        let by_id = if principal.kind == PrincipalKind::Admin {
            sqlx::query(&format!("SELECT {AGENT_COLS} FROM agents WHERE id = $1"))
                .bind(id_or_name)
                .fetch_optional(&self.pool)
                .await
        } else {
            sqlx::query(&format!(
                "SELECT {AGENT_COLS} FROM agents WHERE id = $1 AND principal_id = $2"
            ))
            .bind(id_or_name)
            .bind(&principal.id)
            .fetch_optional(&self.pool)
            .await
        }
        .map_err(AppError::Db)?;
        if let Some(r) = by_id {
            return Ok(row_to_agent(&r));
        }
        // Fall back to the human-friendly name (quick-start / playground friendly).
        let by_name = if principal.kind == PrincipalKind::Admin {
            sqlx::query(&format!("SELECT {AGENT_COLS} FROM agents WHERE name = $1"))
                .bind(id_or_name)
                .fetch_optional(&self.pool)
                .await
        } else {
            sqlx::query(&format!(
                "SELECT {AGENT_COLS} FROM agents WHERE name = $1 AND principal_id = $2"
            ))
            .bind(id_or_name)
            .bind(&principal.id)
            .fetch_optional(&self.pool)
            .await
        }
        .map_err(AppError::Db)?;
        by_name.map(|r| row_to_agent(&r)).ok_or(AppError::NotFound)
    }

    async fn save_agent(&self, agent: &Agent) -> Result<(), AppError> {
        let (text_format, text_verbosity) = match &agent.text {
            Some(t) => (t.format.clone(), t.verbosity.clone()),
            None => (None, None),
        };
        let tools = serde_json::to_string(&agent.tools)
            .map_err(|e| AppError::BadRequest(format!("invalid tools: {e}")))?;
        sqlx::query(
            "UPDATE agents SET name = $2, model = $3, instructions = $4, reasoning_effort = $5, \
             reasoning_summary = $6, multi_agent_enabled = $7, max_concurrent_subagents = $8, \
             service_tier = $9, text_format = $10, text_verbosity = $11, tools = $12, \
             updated_at = now() \
             WHERE id = $1",
        )
        .bind(&agent.id)
        .bind(&agent.name)
        .bind(&agent.model)
        .bind(&agent.instructions)
        .bind(&agent.reasoning.effort)
        .bind(&agent.reasoning.summary)
        .bind(agent.multi_agent.enabled)
        .bind(agent.multi_agent.max_concurrent_subagents.map(|v| v as i32))
        .bind(&agent.service_tier)
        .bind(&text_format)
        .bind(&text_verbosity)
        .bind(&tools)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn delete_agent(&self, principal: &Principal, id: &str) -> Result<String, AppError> {
        let agent = self.get_agent(principal, id).await?;
        sqlx::query("DELETE FROM agents WHERE id = $1")
            .bind(&agent.id)
            .execute(&self.pool)
            .await
            .map_err(AppError::Db)?;
        Ok(agent.id)
    }
}

#[async_trait]
impl SessionStore for PgStore {
    async fn insert_session(
        &self,
        principal: &Principal,
        session: &SessionRow,
    ) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO agent_sessions (id, principal_id, agent_id, instructions, status) \
             VALUES ($1, $2, $3, $4, 'idle')",
        )
        .bind(&session.id)
        .bind(&principal.id)
        .bind(&session.agent_id)
        .bind(&session.instructions)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn list_sessions(
        &self,
        principal: &Principal,
        limit: i64,
    ) -> Result<Vec<SessionRow>, AppError> {
        let rows = if principal.kind == PrincipalKind::Admin {
            sqlx::query(&format!(
                "SELECT {SESSION_COLS} FROM agent_sessions ORDER BY created_at DESC LIMIT $1"
            ))
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(&format!(
                "SELECT {SESSION_COLS} FROM agent_sessions WHERE principal_id = $1 \
                 ORDER BY created_at DESC LIMIT $2"
            ))
            .bind(&principal.id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(AppError::Db)?;
        Ok(rows.iter().map(row_to_session).collect())
    }

    async fn get_session(&self, principal: &Principal, id: &str) -> Result<SessionRow, AppError> {
        let row = if principal.kind == PrincipalKind::Admin {
            sqlx::query(&format!(
                "SELECT {SESSION_COLS} FROM agent_sessions WHERE id = $1"
            ))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
        } else {
            sqlx::query(&format!(
                "SELECT {SESSION_COLS} FROM agent_sessions WHERE id = $1 AND principal_id = $2"
            ))
            .bind(id)
            .bind(&principal.id)
            .fetch_optional(&self.pool)
            .await
        }
        .map_err(AppError::Db)?;
        row.map(|r| row_to_session(&r)).ok_or(AppError::NotFound)
    }

    async fn set_instructions(&self, id: &str, instructions: Option<&str>) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE agent_sessions SET instructions = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(instructions)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn delete_session(&self, id: &str) -> Result<(), AppError> {
        sqlx::query("DELETE FROM session_items WHERE session_id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(AppError::Db)?;
        sqlx::query("DELETE FROM session_turns WHERE session_id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(AppError::Db)?;
        sqlx::query("DELETE FROM agent_sessions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(AppError::Db)?;
        Ok(())
    }

    async fn insert_turn(&self, turn: &TurnRow) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO session_turns (id, principal_id, session_id, agent_id, status) \
             VALUES ($1, $2, $3, $4, 'in_progress')",
        )
        .bind(&turn.id)
        .bind(&turn.principal_id)
        .bind(&turn.session_id)
        .bind(&turn.agent_id)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn finish_turn(
        &self,
        id: &str,
        status: &str,
        output: Option<&str>,
    ) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE session_turns SET status = $2, output = $3, completed_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(status)
        .bind(output)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn list_turns(
        &self,
        principal: &Principal,
        session: &str,
    ) -> Result<Vec<TurnRow>, AppError> {
        let rows = tenant_query(
            &self.pool,
            principal,
            &format!(
                "SELECT {TURN_COLS} FROM session_turns \
                 WHERE session_id = $1 {SCOPE} ORDER BY created_at"
            ),
            &[session],
        )
        .await?;
        Ok(rows.iter().map(row_to_turn).collect())
    }

    async fn get_turn(
        &self,
        principal: &Principal,
        session: &str,
        turn: &str,
    ) -> Result<Option<TurnRow>, AppError> {
        let rows = tenant_query(
            &self.pool,
            principal,
            &format!(
                "SELECT {TURN_COLS} FROM session_turns \
                 WHERE id = $1 AND session_id = $2 {SCOPE}"
            ),
            &[turn, session],
        )
        .await?;
        Ok(rows.first().map(row_to_turn))
    }

    async fn insert_item(&self, item: &ItemRow) -> Result<(), AppError> {
        let content = serde_json::to_string(&item.content)
            .map_err(|e| AppError::BadRequest(format!("invalid item content: {e}")))?;
        sqlx::query(
            "INSERT INTO session_items \
             (id, principal_id, session_id, turn_id, item_type, role, status, content, seq) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&item.id)
        .bind(&item.principal_id)
        .bind(&item.session_id)
        .bind(&item.turn_id)
        .bind(&item.kind)
        .bind(&item.role)
        .bind(&item.status)
        .bind(content)
        .bind(item.seq)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn list_items(
        &self,
        principal: &Principal,
        session: &str,
    ) -> Result<Vec<ItemRow>, AppError> {
        let rows = tenant_query(
            &self.pool,
            principal,
            &format!(
                "SELECT {ITEM_COLS} FROM session_items \
                 WHERE session_id = $1 {SCOPE} ORDER BY seq, created_at"
            ),
            &[session],
        )
        .await?;
        Ok(rows.iter().map(row_to_item).collect())
    }
}

#[async_trait]
impl ApiKeyStore for PgStore {
    async fn insert_key(&self, row: &NewApiKeyRow) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO api_keys (id, key_prefix, key_hash, principal_id, label, scopes, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now())",
        )
        .bind(&row.id)
        .bind(&row.key_prefix)
        .bind(&row.key_hash)
        .bind(&row.principal_id)
        .bind(&row.label)
        .bind(scopes_to_string(ApiKeyScopes::from_tokens(&row.scopes)))
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(())
    }

    async fn list_keys(&self) -> Result<Vec<ApiKeyRow>, AppError> {
        let rows = sqlx::query(
            "SELECT id, key_prefix, principal_id, label, scopes, created_at::text \
             FROM api_keys WHERE revoked_at IS NULL ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(rows
            .iter()
            .map(|r| {
                let scopes: String = r.get("scopes");
                ApiKeyRow {
                    id: r.get("id"),
                    key_prefix: r.get("key_prefix"),
                    principal_id: r.get("principal_id"),
                    label: r.get("label"),
                    scopes: scopes_from_string(&scopes),
                    created_at: r.get("created_at"),
                }
            })
            .collect())
    }

    async fn revoke_key(&self, id: &str) -> Result<Option<String>, AppError> {
        let res = sqlx::query(
            "UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(AppError::Db)?;
        if res.rows_affected() == 0 {
            return Ok(None);
        }
        let row = sqlx::query("SELECT key_hash FROM api_keys WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(AppError::Db)?;
        Ok(row.map(|r| r.get::<String, _>("key_hash")))
    }

    async fn resolve_key(&self, key_hash: &str) -> Result<Option<ResolvedKey>, AppError> {
        let row = sqlx::query(
            "SELECT principal_id, scopes FROM api_keys \
             WHERE key_hash = $1 AND revoked_at IS NULL",
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(AppError::Db)?;
        Ok(row.map(|r| {
            let pid: String = r.get("principal_id");
            let scopes: String = r.get("scopes");
            ResolvedKey {
                principal_id: pid,
                scopes: ApiKeyScopes::from_tokens(&scopes_from_string(&scopes)),
            }
        }))
    }
}

/// Create every table the cloud needs (idempotent) and seed the demo agent.
///
/// pgvector is a hard requirement: [`crate::context::ensure_pgvector`] fails
/// the boot when the `vector` extension is missing.
pub async fn ensure_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    pool.execute(
        "CREATE TABLE IF NOT EXISTS agents (\
            id TEXT PRIMARY KEY, \
            name TEXT NOT NULL, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .await?;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS runs (\
            id TEXT PRIMARY KEY, \
            agent_id TEXT NOT NULL, \
            session TEXT NOT NULL, \
            input TEXT NOT NULL, \
            output TEXT NOT NULL, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .await?;
    // Multi-tenant API keys (metadata only; key plaintext is never stored).
    pool.execute(
        "CREATE TABLE IF NOT EXISTS api_keys (\
            id TEXT PRIMARY KEY, \
            key_prefix TEXT NOT NULL, \
            key_hash TEXT NOT NULL UNIQUE, \
            principal_id TEXT NOT NULL, \
            label TEXT, \
            scopes TEXT NOT NULL, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            revoked_at TIMESTAMPTZ)",
    )
    .await?;
    pool.execute("CREATE UNIQUE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys (key_hash)")
        .await?;
    // Context memory (Postgres + pgvector).
    crate::context::ensure_pgvector(pool)
        .await
        .map_err(sqlx::Error::Protocol)?;
    crate::context::pg::ensure_schema(pool).await?;
    // Attribute runs to the calling principal (audit only).
    pool.execute("ALTER TABLE runs ADD COLUMN IF NOT EXISTS principal_id TEXT")
        .await?;
    // Attribute agents to the owning principal (tenant isolation).
    pool.execute("ALTER TABLE agents ADD COLUMN IF NOT EXISTS principal_id TEXT")
        .await?;
    // --- beta Agents: agent columns + sessions / turns / items ---
    for stmt in [
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS model TEXT",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS instructions TEXT",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS reasoning_effort TEXT",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS reasoning_summary TEXT",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS multi_agent_enabled BOOLEAN NOT NULL DEFAULT false",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS max_concurrent_subagents INTEGER",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS service_tier TEXT DEFAULT 'auto'",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS text_format TEXT",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS text_verbosity TEXT",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS tools TEXT NOT NULL DEFAULT '[]'",
        "ALTER TABLE agents ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ",
        "CREATE TABLE IF NOT EXISTS agent_sessions (\
            id TEXT PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            agent_id TEXT NOT NULL, \
            instructions TEXT, \
            status TEXT NOT NULL DEFAULT 'idle', \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        "CREATE INDEX IF NOT EXISTS idx_agent_sessions_principal ON agent_sessions (principal_id, created_at)",
        "CREATE TABLE IF NOT EXISTS session_turns (\
            id TEXT PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            session_id TEXT NOT NULL, \
            agent_id TEXT NOT NULL, \
            status TEXT NOT NULL, \
            output TEXT, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            completed_at TIMESTAMPTZ)",
        "CREATE INDEX IF NOT EXISTS idx_session_turns_session ON session_turns (principal_id, session_id)",
        "CREATE TABLE IF NOT EXISTS session_items (\
            id TEXT PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            session_id TEXT NOT NULL, \
            turn_id TEXT NOT NULL, \
            item_type TEXT NOT NULL, \
            role TEXT, \
            status TEXT NOT NULL, \
            content TEXT NOT NULL, \
            seq INTEGER NOT NULL DEFAULT 0, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        "CREATE INDEX IF NOT EXISTS idx_session_items_session ON session_items (principal_id, session_id)",
    ] {
        pool.execute(stmt).await?;
    }
    // Seed the default playground agent. Both statements are idempotent: the
    // UPDATE renames the legacy seed (`playground-demo`) in place and is
    // skipped once `agent-demo` exists; the INSERT re-creates a missing one.
    pool.execute(
        "UPDATE agents SET id = 'agent-demo', name = 'Agent Demo' \
         WHERE id = 'playground-demo' \
           AND NOT EXISTS (SELECT 1 FROM agents WHERE id = 'agent-demo')",
    )
    .await?;
    pool.execute(
        "INSERT INTO agents (id, name, principal_id) \
         VALUES ('agent-demo', 'Agent Demo', 'admin') \
         ON CONFLICT (id) DO NOTHING",
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AgentStore;

    fn principal(kind: PrincipalKind) -> Principal {
        Principal {
            id: "tenantA".into(),
            kind,
            scopes: ApiKeyScopes::read_write(),
        }
    }

    /// Regression: the tenant filter must land inside `WHERE`, never after
    /// `ORDER BY` (that produced `argument of AND must be type boolean, not
    /// type timestamp with time zone`).
    #[test]
    fn tenant_scope_is_injected_before_order_by() {
        let base = format!(
            "SELECT {ITEM_COLS} FROM session_items \
             WHERE session_id = $1 {SCOPE} ORDER BY seq, created_at"
        );
        let sql = scope_sql(&base, &principal(PrincipalKind::User), 1);
        assert!(sql.contains("WHERE session_id = $1 AND principal_id = $2 ORDER BY"));
        assert!(!sql.contains(SCOPE));
        // The `AND` binds a column, not the sort expression.
        assert!(sql.find("AND principal_id").unwrap() < sql.find("ORDER BY").unwrap());
    }

    /// The principal id is appended *after* the caller's own params, so its
    /// placeholder number depends on how many are already bound.
    #[test]
    fn tenant_scope_placeholder_follows_bound_params() {
        let base = format!(
            "SELECT {TURN_COLS} FROM session_turns \
             WHERE id = $1 AND session_id = $2 {SCOPE}"
        );
        let sql = scope_sql(&base, &principal(PrincipalKind::User), 2);
        assert!(sql.contains("AND principal_id = $3"));
    }

    /// Admins read across tenants: the marker is removed, not replaced.
    #[test]
    fn admin_scope_removes_the_filter() {
        let base = format!("SELECT {ITEM_COLS} FROM session_items WHERE session_id = $1 {SCOPE}");
        let sql = scope_sql(&base, &principal(PrincipalKind::Admin), 1);
        assert!(!sql.contains("AND principal_id"));
        assert!(!sql.contains(SCOPE));
    }

    /// DB-gated: skipped unless `DATABASE_URL` points at a reachable Postgres
    /// with the `vector` extension. Exercises the real SQL end to end.
    #[tokio::test]
    async fn pg_stores_roundtrip_when_database_url_is_set() {
        let url = match std::env::var("DATABASE_URL") {
            Ok(u) => u,
            Err(_) => {
                eprintln!("skip: DATABASE_URL not set");
                return;
            }
        };
        let pool = match sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skip: DATABASE_URL unreachable: {e}");
                return;
            }
        };
        ensure_schema(&pool)
            .await
            .expect("ensure_schema (needs pgvector)");
        let store = PgStore::new(pool);
        let tenant = Principal {
            id: format!("pg_{}", crate::util::new_id("t")),
            kind: PrincipalKind::User,
            scopes: ApiKeyScopes::read_write(),
        };

        let id = crate::util::new_id("agent");
        let mut agent = Agent {
            id: id.clone(),
            object: AGENT_OBJECT.into(),
            name: Some("pg demo".into()),
            model: Some("gpt-4o-mini".into()),
            instructions: Some("be brief".into()),
            reasoning: AgentReasoning::default(),
            multi_agent: MultiAgentConfig::default(),
            service_tier: "auto".into(),
            text: None,
            tools: vec![],
            created_at: crate::util::now_ms(),
        };
        store.insert_agent(&tenant, &agent).await.unwrap();
        assert_eq!(store.get_agent(&tenant, &id).await.unwrap().id, id);
        assert_eq!(store.get_agent(&tenant, "pg demo").await.unwrap().id, id);
        agent.instructions = Some("updated".into());
        store.save_agent(&agent).await.unwrap();
        assert!(store
            .list_agents(&tenant, 10)
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == id));

        let session = SessionRow {
            id: crate::util::new_id("sess"),
            principal_id: tenant.id.clone(),
            agent_id: id.clone(),
            instructions: None,
            status: "idle".into(),
            created_at: crate::util::now_ms(),
        };
        store.insert_session(&tenant, &session).await.unwrap();
        assert_eq!(
            store.get_session(&tenant, &session.id).await.unwrap().id,
            session.id
        );
        store
            .set_instructions(&session.id, Some("extra"))
            .await
            .unwrap();
        assert_eq!(
            store
                .get_session(&tenant, &session.id)
                .await
                .unwrap()
                .instructions,
            Some("extra".into())
        );

        let turn = TurnRow {
            id: crate::util::new_id("turn"),
            principal_id: tenant.id.clone(),
            session_id: session.id.clone(),
            agent_id: id.clone(),
            status: "in_progress".into(),
            output: None,
            created_at: crate::util::now_ms(),
            completed_at: None,
        };
        store.insert_turn(&turn).await.unwrap();
        store
            .finish_turn(&turn.id, "completed", Some("hi"))
            .await
            .unwrap();
        assert_eq!(
            store
                .get_turn(&tenant, &session.id, &turn.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );
        assert_eq!(
            store.list_turns(&tenant, &session.id).await.unwrap().len(),
            1
        );

        let item = ItemRow {
            id: crate::util::new_id("item"),
            principal_id: tenant.id.clone(),
            session_id: session.id.clone(),
            turn_id: turn.id.clone(),
            kind: "message".into(),
            role: Some("user".into()),
            status: "completed".into(),
            content: serde_json::json!("hello"),
            seq: 0,
            created_at: crate::util::now_ms(),
        };
        store.insert_item(&item).await.unwrap();
        assert_eq!(
            store.list_items(&tenant, &session.id).await.unwrap().len(),
            1
        );

        let key = NewApiKeyRow {
            id: crate::util::new_id("key"),
            key_prefix: "aria-abc".into(),
            key_hash: format!("hash-{}", crate::util::new_id("h")),
            principal_id: tenant.id.clone(),
            label: Some("test".into()),
            scopes: vec!["read".into(), "write".into()],
        };
        store.insert_key(&key).await.unwrap();
        assert!(store.resolve_key(&key.key_hash).await.unwrap().is_some());
        assert!(store
            .list_keys()
            .await
            .unwrap()
            .iter()
            .any(|k| k.id == key.id));
        assert!(store.revoke_key(&key.id).await.unwrap().is_some());
        assert!(store.resolve_key(&key.key_hash).await.unwrap().is_none());
        assert!(store.revoke_key(&key.id).await.unwrap().is_none());

        store.delete_session(&session.id).await.unwrap();
        store.delete_agent(&tenant, &id).await.unwrap();
    }
}
