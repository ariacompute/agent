//! `POST / GET / POST{id} / DELETE /v1/agents[/{agent_id}]`
//!
//! Mirrors the OpenAI beta **Agents** resource. Every row is scoped by
//! `principal_id` (admins see every agent, tenants only their own) — that
//! scoping lives in the [`crate::store`] implementations, not in the handlers.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;

use crate::app::AppState;
use crate::auth::Principal;
use crate::store::{agent_from_request, apply_agent_update};
use crate::types::{Agent, AgentDeleted, CreateAgentRequest, ListResponse, UpdateAgentRequest};
use crate::util::{new_id, now_ms};
use crate::AppError;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

pub fn default_limit() -> i64 {
    20
}

pub async fn create_agent(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<CreateAgentRequest>,
) -> Result<Json<Agent>, AppError> {
    let agent = agent_from_request(new_id("agent"), body, now_ms())?;
    state.agents.insert_agent(&principal, &agent).await?;
    Ok(Json(agent))
}

/// List the agents visible to the caller (admins: all; tenants: their own).
pub async fn list_agents(
    State(state): State<AppState>,
    principal: Principal,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<Agent>>, AppError> {
    let limit = params.limit.clamp(1, 100);
    let agents = state.agents.list_agents(&principal, limit).await?;
    Ok(Json(ListResponse::new(agents)))
}

pub async fn get_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(agent_id): Path<String>,
) -> Result<Json<Agent>, AppError> {
    let agent = state.agents.get_agent(&principal, &agent_id).await?;
    Ok(Json(agent))
}

/// Update an agent. Only the provided fields change (partial update).
pub async fn update_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(agent_id): Path<String>,
    Json(body): Json<UpdateAgentRequest>,
) -> Result<Json<Agent>, AppError> {
    // Ensure the caller can see (and therefore mutate) the agent.
    let mut current = state.agents.get_agent(&principal, &agent_id).await?;
    apply_agent_update(&mut current, body)?;
    state.agents.save_agent(&current).await?;
    Ok(Json(current))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentDeleted>, AppError> {
    let id = state.agents.delete_agent(&principal, &agent_id).await?;
    Ok(Json(AgentDeleted {
        id,
        object: "agent.deleted".into(),
        deleted: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limit_is_twenty() {
        assert_eq!(default_limit(), 20);
    }

    #[test]
    fn list_params_parse_limit() {
        let p: ListParams = serde_json::from_str(r#"{"limit":5}"#).unwrap();
        assert_eq!(p.limit, 5);
        let default: ListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(default.limit, 20);
    }
}
