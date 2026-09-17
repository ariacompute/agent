//! `POST / GET / POST{id} / DELETE /v1/agents[/{agent_id}]`
//!
//! Mirrors the OpenAI beta **Agents** resource. Every row is scoped by
//! `principal_id`: admins see every agent, tenants only their own.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::Value;
use sqlx::Row;

use crate::types::{
    Agent, AgentDeleted, AgentReasoning, AgentText, CreateAgentRequest, ListResponse,
    MultiAgentConfig, UpdateAgentRequest, AGENT_OBJECT,
};
use crate::{AppError, AppState, Principal, PrincipalKind};

/// Column list shared by every agent read. `created_at` is returned as epoch
/// millis so no chrono/time dependency is needed.
const AGENT_COLS: &str = "id, name, model, instructions, reasoning_effort, reasoning_summary, \
     multi_agent_enabled, max_concurrent_subagents, service_tier, text_format, text_verbosity, \
     tools, (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at";

#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    20
}

pub async fn create_agent(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<CreateAgentRequest>,
) -> Result<Json<Agent>, AppError> {
    let id = new_id("agent");
    let reasoning = body.reasoning.clone().unwrap_or_default();
    let multi_agent = body.multi_agent.clone().unwrap_or_default();
    let service_tier = body.service_tier.clone().unwrap_or_else(|| "auto".into());
    let (text_format, text_verbosity) = match &body.text {
        Some(t) => (t.format.clone(), t.verbosity.clone()),
        None => (None, None),
    };
    let tools = serde_json::to_string(&body.tools.clone().unwrap_or_default())
        .map_err(|e| AppError::BadRequest(format!("invalid tools: {e}")))?;

    sqlx::query(
        "INSERT INTO agents \
         (id, name, model, instructions, reasoning_effort, reasoning_summary, \
          multi_agent_enabled, max_concurrent_subagents, service_tier, text_format, \
          text_verbosity, tools, principal_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
    )
    .bind(&id)
    .bind(&body.name)
    .bind(&body.model)
    .bind(&body.instructions)
    .bind(&reasoning.effort)
    .bind(&reasoning.summary)
    .bind(multi_agent.enabled)
    .bind(multi_agent.max_concurrent_subagents.map(|v| v as i32))
    .bind(&service_tier)
    .bind(&text_format)
    .bind(&text_verbosity)
    .bind(&tools)
    .bind(&principal.id)
    .execute(&state.pool)
    .await
    .map_err(AppError::Db)?;

    Ok(Json(Agent {
        id,
        object: AGENT_OBJECT.into(),
        name: body.name,
        model: body.model,
        instructions: body.instructions,
        reasoning,
        multi_agent,
        service_tier,
        text: body.text,
        tools: body.tools.unwrap_or_default(),
        created_at: now_ms(),
    }))
}

/// List the agents visible to the caller (admins: all; tenants: their own).
pub async fn list_agents(
    State(state): State<AppState>,
    principal: Principal,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<Agent>>, AppError> {
    let limit = params.limit.clamp(1, 100);
    let rows = if principal.kind == PrincipalKind::Admin {
        sqlx::query(&format!(
            "SELECT {AGENT_COLS} FROM agents ORDER BY created_at DESC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&state.pool)
        .await
    } else {
        sqlx::query(&format!(
            "SELECT {AGENT_COLS} FROM agents WHERE principal_id = $1 \
             ORDER BY created_at DESC LIMIT $2"
        ))
        .bind(&principal.id)
        .bind(limit)
        .fetch_all(&state.pool)
        .await
    }
    .map_err(AppError::Db)?;
    Ok(Json(ListResponse::new(
        rows.iter().map(row_to_agent).collect(),
    )))
}

pub async fn get_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(agent_id): Path<String>,
) -> Result<Json<Agent>, AppError> {
    let row = fetch_agent_row(&state, &principal, &agent_id).await?;
    Ok(Json(row_to_agent(&row)))
}

/// Update an agent. Only the provided fields change (partial update).
pub async fn update_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(agent_id): Path<String>,
    Json(body): Json<UpdateAgentRequest>,
) -> Result<Json<Agent>, AppError> {
    // Ensure the caller can see (and therefore mutate) the agent.
    let row = fetch_agent_row(&state, &principal, &agent_id).await?;
    let mut current = row_to_agent(&row);

    if let Some(name) = body.name {
        current.name = Some(name);
    }
    if let Some(model) = body.model {
        current.model = Some(model);
    }
    if let Some(instructions) = body.instructions {
        current.instructions = Some(instructions);
    }
    if let Some(r) = body.reasoning {
        current.reasoning = r;
    }
    if let Some(m) = body.multi_agent {
        current.multi_agent = m;
    }
    if let Some(t) = body.service_tier {
        current.service_tier = t;
    }
    if body.text.is_some() {
        current.text = body.text;
    }
    if let Some(tools) = body.tools {
        current.tools = tools;
    }

    let (text_format, text_verbosity) = match &current.text {
        Some(t) => (t.format.clone(), t.verbosity.clone()),
        None => (None, None),
    };
    let tools = serde_json::to_string(&current.tools)
        .map_err(|e| AppError::BadRequest(format!("invalid tools: {e}")))?;
    sqlx::query(
        "UPDATE agents SET name = $2, model = $3, instructions = $4, reasoning_effort = $5, \
         reasoning_summary = $6, multi_agent_enabled = $7, max_concurrent_subagents = $8, \
         service_tier = $9, text_format = $10, text_verbosity = $11, tools = $12, \
         updated_at = now() \
         WHERE id = $1",
    )
    .bind(&current.id)
    .bind(&current.name)
    .bind(&current.model)
    .bind(&current.instructions)
    .bind(&current.reasoning.effort)
    .bind(&current.reasoning.summary)
    .bind(current.multi_agent.enabled)
    .bind(
        current
            .multi_agent
            .max_concurrent_subagents
            .map(|v| v as i32),
    )
    .bind(&current.service_tier)
    .bind(&text_format)
    .bind(&text_verbosity)
    .bind(&tools)
    .execute(&state.pool)
    .await
    .map_err(AppError::Db)?;

    Ok(Json(current))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentDeleted>, AppError> {
    let row = fetch_agent_row(&state, &principal, &agent_id).await?;
    let id: String = row.get("id");
    sqlx::query("DELETE FROM agents WHERE id = $1")
        .bind(&id)
        .execute(&state.pool)
        .await
        .map_err(AppError::Db)?;
    Ok(Json(AgentDeleted {
        id,
        object: "agent.deleted".into(),
        deleted: true,
    }))
}

/// Load one agent row, scoped to the caller (`principal_id` filter for
/// tenants). `id_or_name` accepts either the id or the human-friendly name.
pub(crate) async fn fetch_agent_row(
    state: &AppState,
    principal: &Principal,
    id_or_name: &str,
) -> Result<sqlx::postgres::PgRow, AppError> {
    let by_id = if principal.kind == PrincipalKind::Admin {
        sqlx::query(&format!("SELECT {AGENT_COLS} FROM agents WHERE id = $1"))
            .bind(id_or_name)
            .fetch_optional(&state.pool)
            .await
    } else {
        sqlx::query(&format!(
            "SELECT {AGENT_COLS} FROM agents WHERE id = $1 AND principal_id = $2"
        ))
        .bind(id_or_name)
        .bind(&principal.id)
        .fetch_optional(&state.pool)
        .await
    }
    .map_err(AppError::Db)?;
    if let Some(r) = by_id {
        return Ok(r);
    }
    // Fall back to the human-friendly name (quick-start / playground friendly).
    let by_name = if principal.kind == PrincipalKind::Admin {
        sqlx::query(&format!("SELECT {AGENT_COLS} FROM agents WHERE name = $1"))
            .bind(id_or_name)
            .fetch_optional(&state.pool)
            .await
    } else {
        sqlx::query(&format!(
            "SELECT {AGENT_COLS} FROM agents WHERE name = $1 AND principal_id = $2"
        ))
        .bind(id_or_name)
        .bind(&principal.id)
        .fetch_optional(&state.pool)
        .await
    }
    .map_err(AppError::Db)?;
    by_name.ok_or(AppError::NotFound)
}

/// Convenience for the session layer: resolve an agent by id or name.
pub(crate) async fn fetch_agent(
    state: &AppState,
    principal: &Principal,
    id_or_name: &str,
) -> Result<Agent, AppError> {
    let row = fetch_agent_row(state, principal, id_or_name).await?;
    Ok(row_to_agent(&row))
}

fn row_to_agent(r: &sqlx::postgres::PgRow) -> Agent {
    let tools: String = r.get::<Option<String>, _>("tools").unwrap_or_default();
    let tools: Vec<Value> = serde_json::from_str(&tools).unwrap_or_default();
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

/// `agent_<hex>` — matches the OpenAI id shape closely enough for clients.
pub(crate) fn new_id(prefix: &str) -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    let raw: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}_{raw}")
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_prefixed_and_unique() {
        let a = new_id("agent");
        let b = new_id("agent");
        assert!(a.starts_with("agent_"));
        assert_ne!(a, b);
    }

    #[test]
    fn default_limit_is_twenty() {
        assert_eq!(default_limit(), 20);
    }

    #[test]
    fn list_params_parse_limit() {
        let p: ListParams = serde_json::from_str(r#"{"limit":5}"#).unwrap();
        assert_eq!(p.limit, 5);
    }
}
