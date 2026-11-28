//! A2A server surface: discovery cards, the JSON-RPC endpoint and the replay
//! stream.
//!
//! Layering is the security boundary. Cards are **public** (an orchestrator must
//! be able to discover an agent before it has a key), so their router is merged
//! after the auth layer exactly like `/healthz`. Every JSON-RPC route is
//! registered before that layer and therefore inherits Bearer auth; scopes are
//! checked again inside the dispatcher (`read` / `write`), because a key can be
//! read-only.
//!
//! The surface is byte-compatible with agenthub's client
//! (`agenthub/internal/a2a` + `adapter/agent.go`): `parts` carry both `kind` and
//! `type`, frames carry `kind` and the legacy `event` name, and a method-level
//! error travels inside a **200** response so the caller can read the code
//! instead of classifying the whole endpoint as unreachable.

pub mod card;
pub mod executor;
pub mod hitl;
pub mod pg;
pub mod push;
pub mod rpc;
pub mod runtime;
pub mod store;

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;

use crate::app::AppState;
use crate::auth::Principal;

use self::card::CardOverrides;
use self::push::{NoopPush, PushNotifier};
use self::runtime::A2aRuntime;
use self::store::{A2aTaskStore, MemoryA2aTasks};

/// Deployment configuration for the A2A surface.
#[derive(Debug, Clone, Default)]
pub struct A2aConfig {
    /// `A2A_DEFAULT_AGENT`: the agent the bare `/a2a` endpoint serves. `None`
    /// makes `/a2a` answer `agent_not_found` and the well-known card 404.
    pub default_agent: Option<String>,
    /// `A2A_PUBLIC_URL`: the origin callers reach this server on. It is what the
    /// card advertises, so it cannot be inferred from the request.
    pub public_url: String,
    /// `A2A_CARDS_FILE` overrides.
    pub cards: CardOverrides,
}

/// Everything the A2A routes need.
#[derive(Clone)]
pub struct A2aState {
    pub runtime: Arc<A2aRuntime>,
    pub tasks: Arc<dyn A2aTaskStore>,
    pub push: Arc<dyn PushNotifier>,
    pub config: Arc<A2aConfig>,
    /// `false` when the deployment did not configure A2A at all: the routes
    /// still exist (so a probe gets a clear answer) but nothing can be started.
    pub enabled: bool,
}

impl A2aState {
    pub fn disabled() -> Self {
        Self {
            runtime: Arc::new(A2aRuntime::new()),
            tasks: Arc::new(MemoryA2aTasks::new()),
            push: Arc::new(NoopPush),
            config: Arc::new(A2aConfig::default()),
            enabled: false,
        }
    }
}

/// The public discovery routes. Merged *after* the auth layer.
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/.well-known/agent-card.json", get(service_card))
        .route("/.well-known/agent.json", get(service_card))
        .route("/a2a/agent-card", get(service_card))
        .route(
            "/v1/agents/:agent_id/.well-known/agent-card.json",
            get(agent_card),
        )
        .route("/v1/agents/:agent_id/agent-card.json", get(agent_card))
}

/// The authenticated A2A routes. Registered before the auth layer.
pub fn protected_router() -> Router<AppState> {
    Router::new()
        .route("/a2a", post(rpc::handle_default))
        .route("/v1/agents/:agent_id/a2a", post(rpc::handle_agent))
        .route("/a2a/stream", get(rpc::stream))
}

/// The default agent's card (or 404 with a machine-readable body).
pub async fn service_card(State(state): State<AppState>) -> Response {
    let Some(agent_id) = state.a2a.config.default_agent.clone() else {
        return missing_card("no default A2A agent is configured");
    };
    match card_for(&state, &agent_id, "/a2a").await {
        Some(card) => Json(card).into_response(),
        None => missing_card("default A2A agent not found"),
    }
}

/// One agent's card.
pub async fn agent_card(State(state): State<AppState>, Path(agent_id): Path<String>) -> Response {
    let path = format!("/v1/agents/{agent_id}/a2a");
    match card_for(&state, &agent_id, &path).await {
        Some(card) => Json(card).into_response(),
        None => missing_card("agent not found"),
    }
}

/// Resolve the agent and project its card.
///
/// The lookup uses the admin principal: the card is public, and an agent that
/// exists but belongs to another tenant must still be describable (its name is
/// already public by virtue of being discoverable). Nothing else is read.
async fn card_for(
    state: &AppState,
    agent_id: &str,
    path: &str,
) -> Option<aria_agent_a2a::AgentCard> {
    if !state.a2a.enabled {
        return None;
    }
    let agent = state
        .agents
        .get_agent(&Principal::admin(), agent_id)
        .await
        .ok()?;
    let name = agent.name.clone().unwrap_or_else(|| agent.id.clone());
    Some(card::build_card(
        &state.a2a.config.public_url,
        &name,
        path,
        &state.a2a.config.cards,
    ))
}

fn missing_card(reason: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"code": "agent_not_found", "message": reason}})),
    )
        .into_response()
}
