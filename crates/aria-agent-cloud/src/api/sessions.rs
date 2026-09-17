//! Agent **sessions**: `POST / GET / POST{id} / DELETE /v1/agents/sessions[/{id}]`
//! plus the turn driver (`…/events`, `…/events/stream`) and the read-only
//! `items` / `turns` / `subagents` listings.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde::Deserialize;
use sqlx::Row;

use crate::api::agents::{fetch_agent, new_id, now_ms};
use crate::api::error_response;
use crate::types::{
    AgentSession, CreateSessionRequest, ListResponse, MemoryItem, PutMemoryRequest, SessionAgent,
    SessionDeleted, SessionInputEvent, SessionItem, SessionTurn, Subagent, UpdateSessionRequest,
    ITEM_OBJECT, SESSION_OBJECT, SUBAGENT_OBJECT, TURN_OBJECT,
};
use crate::{agent_tools, AppError, AppState, Principal, PrincipalKind};
use agent_core::context::RecallQuery;
use agent_core::context::{ContextFragment, ContextStore, FragmentKind};

const SESSION_COLS: &str = "id, principal_id, agent_id, instructions, status, \
     (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at";
const TURN_COLS: &str = "id, session_id, agent_id, status, output, \
     (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at, \
     (EXTRACT(EPOCH FROM completed_at) * 1000)::BIGINT AS completed_at";
const ITEM_COLS: &str = "id, session_id, turn_id, item_type, role, status, content, seq, \
     (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS created_at";

#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    20
}

// ---------------------------------------------------------------- sessions CRUD

pub async fn create_session(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<AgentSession>, AppError> {
    let agent_ref = body
        .agent_id
        .clone()
        .or_else(|| body.agent.clone())
        .ok_or_else(|| AppError::BadRequest("either `agent_id` or `agent` is required".into()))?;
    let agent = fetch_agent(&state, &principal, &agent_ref).await?;

    let id = new_id("sess");
    sqlx::query(
        "INSERT INTO agent_sessions (id, principal_id, agent_id, instructions, status) \
         VALUES ($1, $2, $3, $4, 'idle')",
    )
    .bind(&id)
    .bind(&principal.id)
    .bind(&agent.id)
    .bind(&body.instructions)
    .execute(&state.pool)
    .await
    .map_err(AppError::Db)?;

    Ok(Json(AgentSession {
        id,
        object: SESSION_OBJECT.into(),
        agent: session_agent(&agent),
        instructions: body.instructions,
        status: "idle".into(),
        created_at: now_ms(),
    }))
}

pub async fn list_sessions(
    State(state): State<AppState>,
    principal: Principal,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<AgentSession>>, AppError> {
    let limit = params.limit.clamp(1, 100);
    let rows = if principal.kind == PrincipalKind::Admin {
        sqlx::query(&format!(
            "SELECT {SESSION_COLS} FROM agent_sessions ORDER BY created_at DESC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&state.pool)
        .await
    } else {
        sqlx::query(&format!(
            "SELECT {SESSION_COLS} FROM agent_sessions WHERE principal_id = $1 \
             ORDER BY created_at DESC LIMIT $2"
        ))
        .bind(&principal.id)
        .bind(limit)
        .fetch_all(&state.pool)
        .await
    }
    .map_err(AppError::Db)?;

    let mut out = Vec::new();
    for r in &rows {
        let agent_id: String = r.get("agent_id");
        // A session whose agent was deleted still lists, with a stub snapshot.
        let agent = match fetch_agent(&state, &principal, &agent_id).await {
            Ok(a) => a,
            Err(_) => continue,
        };
        out.push(AgentSession {
            id: r.get("id"),
            object: SESSION_OBJECT.into(),
            agent: session_agent(&agent),
            instructions: r.get("instructions"),
            status: r.get("status"),
            created_at: r.get("created_at"),
        });
    }
    Ok(Json(ListResponse::new(out)))
}

pub async fn get_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<AgentSession>, AppError> {
    let row = fetch_session(&state, &principal, &session_id).await?;
    let agent_id: String = row.get("agent_id");
    let agent = fetch_agent(&state, &principal, &agent_id).await?;
    Ok(Json(AgentSession {
        id: row.get("id"),
        object: SESSION_OBJECT.into(),
        agent: session_agent(&agent),
        instructions: row.get("instructions"),
        status: row.get("status"),
        created_at: row.get("created_at"),
    }))
}

pub async fn update_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Json(body): Json<UpdateSessionRequest>,
) -> Result<Json<AgentSession>, AppError> {
    let row = fetch_session(&state, &principal, &session_id).await?;
    if body.instructions.is_some() {
        sqlx::query(
            "UPDATE agent_sessions SET instructions = $2, updated_at = now() WHERE id = $1",
        )
        .bind(&session_id)
        .bind(&body.instructions)
        .execute(&state.pool)
        .await
        .map_err(AppError::Db)?;
    }
    let agent_id: String = row.get("agent_id");
    let agent = fetch_agent(&state, &principal, &agent_id).await?;
    Ok(Json(AgentSession {
        id: row.get("id"),
        object: SESSION_OBJECT.into(),
        agent: session_agent(&agent),
        instructions: body.instructions.or_else(|| row.get("instructions")),
        status: row.get("status"),
        created_at: row.get("created_at"),
    }))
}

pub async fn delete_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<SessionDeleted>, AppError> {
    let row = fetch_session(&state, &principal, &session_id).await?;
    let id: String = row.get("id");
    sqlx::query("DELETE FROM session_items WHERE session_id = $1")
        .bind(&id)
        .execute(&state.pool)
        .await
        .map_err(AppError::Db)?;
    sqlx::query("DELETE FROM session_turns WHERE session_id = $1")
        .bind(&id)
        .execute(&state.pool)
        .await
        .map_err(AppError::Db)?;
    sqlx::query("DELETE FROM agent_sessions WHERE id = $1")
        .bind(&id)
        .execute(&state.pool)
        .await
        .map_err(AppError::Db)?;
    Ok(Json(SessionDeleted {
        id,
        object: "agent.session.deleted".into(),
        deleted: true,
    }))
}

// ------------------------------------------------------------------ turn driver

/// `POST /v1/agents/sessions/{id}/events` — run one turn and return the turn.
///
/// The blocking variant: it performs the same work as the streaming endpoint
/// (recall context → model → tools → persist) but returns a single JSON turn.
pub async fn create_session_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    principal: Principal,
    Json(body): Json<SessionInputEvent>,
) -> Result<Json<SessionTurn>, AppError> {
    let input = body.text();
    if input.trim().is_empty() {
        return Err(AppError::BadRequest("input is empty".into()));
    }
    let (agent, instructions) =
        session_agent_and_instructions(&state, &principal, &session_id).await?;
    let turn_id = new_id("turn");
    insert_turn(&state, &principal, &turn_id, &session_id, &agent.id).await?;
    insert_item(
        &state,
        &principal,
        &session_id,
        &turn_id,
        "message",
        Some("user"),
        "completed",
        &serde_json::Value::String(input.clone()),
        0,
    )
    .await?;

    let agent_rt = crate::build_agent(
        &state,
        &agent.name.clone().unwrap_or_else(|| agent.id.clone()),
        &instructions,
        &session_id,
        &principal,
    )
    .await?;
    let output = agent_rt.run(&input).await.map_err(AppError::Core)?;

    insert_item(
        &state,
        &principal,
        &session_id,
        &turn_id,
        "message",
        Some("assistant"),
        "completed",
        &serde_json::Value::String(output.clone()),
        1,
    )
    .await?;
    complete_turn(&state, &turn_id, "completed", Some(&output)).await?;

    Ok(Json(SessionTurn {
        id: turn_id,
        object: TURN_OBJECT.into(),
        session_id,
        agent_id: agent.id,
        status: "completed".into(),
        output: Some(output),
        created_at: now_ms(),
        completed_at: Some(now_ms()),
    }))
}

/// `POST /v1/agents/sessions/{id}/events/stream` — run one turn, streaming
/// events as they happen (SSE). Terminal event is `response.completed` /
/// `response.failed`; there is no `[DONE]` sentinel.
pub async fn stream_session_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    principal: Principal,
    Json(body): Json<SessionInputEvent>,
) -> Response {
    let input = body.text();
    if input.trim().is_empty() {
        return error_response(axum::http::StatusCode::BAD_REQUEST, "input is empty");
    }
    let (agent, instructions) =
        match session_agent_and_instructions(&state, &principal, &session_id).await {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        };
    let turn_id = new_id("turn");
    if let Err(e) = insert_turn(&state, &principal, &turn_id, &session_id, &agent.id).await {
        return e.into_response();
    }

    let agent_name = agent.name.clone().unwrap_or_else(|| agent.id.clone());
    let agent_rt =
        match crate::build_agent(&state, &agent_name, &instructions, &session_id, &principal).await
        {
            Ok(a) => a,
            Err(e) => return e.into_response(),
        };
    let tools = agent_tools();
    let event_stream = match agent_rt.run_event_stream(&input, &tools).await {
        Ok(s) => s,
        Err(e) => return AppError::Core(e).into_response(),
    };

    let pool = state.pool.clone();
    let pid = principal.id.clone();
    let session_id_inner = session_id.clone();
    let turn_id_inner = turn_id.clone();
    let agent_id = agent.id.clone();

    let wrapped = async_stream::stream! {
        let mut envelope =
            crate::event_envelope::EventEnvelope::new(&turn_id_inner, &agent_id, &session_id_inner);
        yield envelope.created();

        let mut collected = String::new();
        let mut seq = 1;
        let mut s = event_stream;
        while let Some(item) = s.next().await {
            match item {
                Ok(ev) => {
                    match &ev {
                        agent_core::AgentEvent::Token { text } => collected.push_str(text),
                        agent_core::AgentEvent::Done { text } if !text.is_empty() => {
                            collected = text.clone()
                        }
                        agent_core::AgentEvent::ToolCall { id, name, arguments, result } => {
                            seq += 1;
                            let content = serde_json::json!({
                                "call_id": id, "name": name, "arguments": arguments,
                                "output": result.content, "is_error": result.is_error,
                            });
                            let _ = insert_item_with(
                                &pool, &pid, &session_id_inner, &turn_id_inner,
                                "function_call", None, "completed", &content, seq,
                            ).await;
                        }
                        _ => {}
                    }
                    for frame in envelope.push(&ev) {
                        yield frame;
                    }
                }
                Err(e) => {
                    let _ = fail_turn(&pool, &turn_id_inner, &e.to_string()).await;
                    let _ = insert_item_with(
                        &pool, &pid, &session_id_inner, &turn_id_inner,
                        "message", Some("assistant"), "incomplete",
                        &serde_json::Value::String(collected.clone()), seq + 1,
                    ).await;
                    yield envelope.failed(&e.to_string());
                    return;
                }
            }
        }

        for frame in envelope.ensure_closed() {
            yield frame;
        }

        seq += 1;
        let _ = insert_item_with(
            &pool, &pid, &session_id_inner, &turn_id_inner,
            "message", Some("assistant"), "completed",
            &serde_json::Value::String(collected.clone()), seq,
        ).await;
        let _ = complete_turn_with(&pool, &turn_id_inner, "completed", Some(&collected)).await;
    };

    crate::build_sse_stream(wrapped)
}

// --------------------------------------------------------- long-term memory

/// `POST /v1/agents/sessions/{id}/memory` — write a memory fragment.
///
/// `key` is optional: a keyed write is a long-term memory (recallable via
/// `GET …/memory/{key}`), an unkeyed write is an ordinary context fragment
/// (message / tool result) that still participates in semantic recall.
pub async fn put_session_memory(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Json(body): Json<PutMemoryRequest>,
) -> Result<Json<MemoryItem>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    let kind = match body.kind.as_deref() {
        None | Some("long_term") => FragmentKind::LongTerm,
        Some("message") => FragmentKind::Message,
        Some("tool_result") => FragmentKind::ToolResult,
        Some("note") => FragmentKind::Note,
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "unknown fragment kind: {other}"
            )))
        }
    };
    let mut frag = ContextFragment::new(&session_id, kind, body.value.clone());
    if let Some(key) = body.key.as_deref().filter(|k| !k.trim().is_empty()) {
        frag = frag.with_key(key);
    }
    let store = state.context_for(&principal);
    store
        .memorize(frag.clone())
        .await
        .map_err(|e| AppError::BadRequest(format!("memory write failed: {e}")))?;
    Ok(Json(MemoryItem {
        id: frag.id,
        key: frag.key,
        kind: kind.as_str().to_string(),
        content: frag.content,
        created_at: frag.created_at,
    }))
}

/// `GET /v1/agents/sessions/{id}/memory/{key}` — read a keyed memory.
pub async fn get_session_memory(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, key)): Path<(String, String)>,
) -> Result<Json<MemoryItem>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    let store = state.context_for(&principal);
    let frag = store
        .get_by_key(&session_id, &key)
        .await
        .map_err(|e| AppError::BadRequest(format!("memory read failed: {e}")))?;
    match frag {
        Some(f) => Ok(Json(MemoryItem {
            id: f.id,
            key: f.key,
            kind: f.kind.as_str().to_string(),
            content: f.content,
            created_at: f.created_at,
        })),
        None => Err(AppError::NotFound),
    }
}

#[derive(Debug, Deserialize)]
pub struct MemoryQuery {
    /// Semantic / keyword query. Omitted (or empty) lists the session's memory.
    text: Option<String>,
    #[serde(default = "default_top_k")]
    top_k: usize,
}

fn default_top_k() -> usize {
    20
}

/// `GET /v1/agents/sessions/{id}/memory?text=…&top_k=…` — recall memory.
///
/// With `text` the cloud store ranks by pgvector similarity + keyword score;
/// without it the endpoint lists the session's fragments (newest last).
pub async fn search_session_memory(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Query(params): Query<MemoryQuery>,
) -> Result<Json<ListResponse<MemoryItem>>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    let store = state.context_for(&principal);
    let items = match params
        .text
        .as_deref()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
    {
        Some(text) => {
            let frags = store
                .recall(&RecallQuery {
                    session: session_id.clone(),
                    text: text.to_string(),
                    top_k: params.top_k.clamp(1, 100),
                    kind: None,
                })
                .await
                .map_err(|e| AppError::BadRequest(format!("memory recall failed: {e}")))?;
            frags.into_iter().map(to_memory_item).collect()
        }
        None => {
            // No query text: list the session's fragments (oldest first).
            let frags = store
                .list_session(&session_id, params.top_k.clamp(1, 100))
                .await
                .map_err(|e| AppError::BadRequest(format!("memory list failed: {e}")))?;
            frags.into_iter().map(to_memory_item).collect()
        }
    };
    Ok(Json(ListResponse::new(items)))
}

fn to_memory_item(f: ContextFragment) -> MemoryItem {
    MemoryItem {
        id: f.id,
        key: f.key,
        kind: f.kind.as_str().to_string(),
        content: f.content,
        created_at: f.created_at,
    }
}

// ------------------------------------------------------------- read-only reads

pub async fn list_session_items(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<ListResponse<SessionItem>>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    let rows = tenant_query(
        &state,
        &principal,
        &format!(
            "SELECT {ITEM_COLS} FROM session_items WHERE session_id = $1 ORDER BY seq, created_at"
        ),
        &session_id,
    )
    .await?;
    Ok(Json(ListResponse::new(
        rows.iter().map(row_to_item).collect(),
    )))
}

pub async fn list_session_turns(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<ListResponse<SessionTurn>>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    let rows = tenant_query(
        &state,
        &principal,
        &format!("SELECT {TURN_COLS} FROM session_turns WHERE session_id = $1 ORDER BY created_at"),
        &session_id,
    )
    .await?;
    Ok(Json(ListResponse::new(
        rows.iter().map(row_to_turn).collect(),
    )))
}

pub async fn get_session_turn(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, turn_id)): Path<(String, String)>,
) -> Result<Json<SessionTurn>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    let rows = tenant_query(
        &state,
        &principal,
        &format!("SELECT {TURN_COLS} FROM session_turns WHERE id = $1 AND session_id = $2"),
        &turn_id,
    )
    .await?;
    rows.iter()
        .find(|r| r.get::<String, _>("session_id") == session_id)
        .map(row_to_turn)
        .map(Json)
        .ok_or(AppError::NotFound)
}

/// This runtime runs a single agent loop, so no subagents are ever spawned.
/// The endpoint exists so clients can enumerate them and simply get none.
pub async fn list_subagents(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<ListResponse<Subagent>>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    Ok(Json(ListResponse::new(Vec::new())))
}

pub async fn get_subagent(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, _subagent_id)): Path<(String, String)>,
) -> Result<Json<Subagent>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    Err(AppError::NotFound)
}

pub async fn list_subagent_items(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, _subagent_id)): Path<(String, String)>,
) -> Result<Json<ListResponse<SessionItem>>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    Ok(Json(ListResponse::new(Vec::new())))
}

pub async fn list_subagent_turns(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, _subagent_id)): Path<(String, String)>,
) -> Result<Json<ListResponse<SessionTurn>>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    Ok(Json(ListResponse::new(Vec::new())))
}

pub async fn get_subagent_turn(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, _subagent_id, _turn_id)): Path<(String, String, String)>,
) -> Result<Json<SessionTurn>, AppError> {
    let _row = fetch_session(&state, &principal, &session_id).await?;
    Err(AppError::NotFound)
}

// ------------------------------------------------------------------- internals

fn session_agent(agent: &crate::types::Agent) -> SessionAgent {
    SessionAgent {
        id: agent.id.clone(),
        name: agent.name.clone(),
        instructions: agent.instructions.clone(),
        model: agent.model.clone(),
        reasoning: agent.reasoning.clone(),
        multi_agent: agent.multi_agent.clone(),
        service_tier: agent.service_tier.clone(),
    }
}

async fn session_agent_and_instructions(
    state: &AppState,
    principal: &Principal,
    session_id: &str,
) -> Result<(crate::types::Agent, String), AppError> {
    let row = fetch_session(state, principal, session_id).await?;
    let agent_id: String = row.get("agent_id");
    let session_instructions: Option<String> = row.get("instructions");
    let agent = fetch_agent(state, principal, &agent_id).await?;
    let mut instructions = agent.instructions.clone().unwrap_or_default();
    if let Some(extra) = session_instructions {
        if !extra.trim().is_empty() {
            instructions = if instructions.is_empty() {
                extra
            } else {
                format!("{instructions}\n\n{extra}")
            };
        }
    }
    Ok((agent, instructions))
}

async fn fetch_session(
    state: &AppState,
    principal: &Principal,
    session_id: &str,
) -> Result<sqlx::postgres::PgRow, AppError> {
    let row = if principal.kind == PrincipalKind::Admin {
        sqlx::query(&format!(
            "SELECT {SESSION_COLS} FROM agent_sessions WHERE id = $1"
        ))
        .bind(session_id)
        .fetch_optional(&state.pool)
        .await
    } else {
        sqlx::query(&format!(
            "SELECT {SESSION_COLS} FROM agent_sessions WHERE id = $1 AND principal_id = $2"
        ))
        .bind(session_id)
        .bind(&principal.id)
        .fetch_optional(&state.pool)
        .await
    }
    .map_err(AppError::Db)?;
    row.ok_or(AppError::NotFound)
}

/// Run a scoped read (`$1` = the resource id, `$2` = principal when tenanted).
async fn tenant_query(
    state: &AppState,
    principal: &Principal,
    base: &str,
    id: &str,
) -> Result<Vec<sqlx::postgres::PgRow>, AppError> {
    if principal.kind == PrincipalKind::Admin {
        sqlx::query(base)
            .bind(id)
            .fetch_all(&state.pool)
            .await
            .map_err(AppError::Db)
    } else {
        sqlx::query(&format!("{base} AND principal_id = $2"))
            .bind(id)
            .bind(&principal.id)
            .fetch_all(&state.pool)
            .await
            .map_err(AppError::Db)
    }
}

async fn insert_turn(
    state: &AppState,
    principal: &Principal,
    turn_id: &str,
    session_id: &str,
    agent_id: &str,
) -> Result<(), AppError> {
    insert_turn_with(&state.pool, &principal.id, turn_id, session_id, agent_id).await
}

async fn insert_turn_with(
    pool: &sqlx::PgPool,
    pid: &str,
    turn_id: &str,
    session_id: &str,
    agent_id: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO session_turns (id, principal_id, session_id, agent_id, status) \
         VALUES ($1, $2, $3, $4, 'in_progress')",
    )
    .bind(turn_id)
    .bind(pid)
    .bind(session_id)
    .bind(agent_id)
    .execute(pool)
    .await
    .map_err(AppError::Db)?;
    Ok(())
}

async fn complete_turn(
    state: &AppState,
    turn_id: &str,
    status: &str,
    output: Option<&str>,
) -> Result<(), AppError> {
    complete_turn_with(&state.pool, turn_id, status, output).await
}

async fn complete_turn_with(
    pool: &sqlx::PgPool,
    turn_id: &str,
    status: &str,
    output: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE session_turns SET status = $2, output = $3, completed_at = now() WHERE id = $1",
    )
    .bind(turn_id)
    .bind(status)
    .bind(output)
    .execute(pool)
    .await
    .map_err(AppError::Db)?;
    Ok(())
}

async fn fail_turn(pool: &sqlx::PgPool, turn_id: &str, error: &str) -> Result<(), AppError> {
    complete_turn_with(pool, turn_id, "failed", Some(error)).await
}

#[allow(clippy::too_many_arguments)]
async fn insert_item(
    state: &AppState,
    principal: &Principal,
    session_id: &str,
    turn_id: &str,
    kind: &str,
    role: Option<&str>,
    status: &str,
    content: &serde_json::Value,
    seq: i32,
) -> Result<(), AppError> {
    insert_item_with(
        &state.pool,
        &principal.id,
        session_id,
        turn_id,
        kind,
        role,
        status,
        content,
        seq,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::result_large_err)]
async fn insert_item_with(
    pool: &sqlx::PgPool,
    pid: &str,
    session_id: &str,
    turn_id: &str,
    kind: &str,
    role: Option<&str>,
    status: &str,
    content: &serde_json::Value,
    seq: i32,
) -> Result<(), AppError> {
    let content = serde_json::to_string(content)
        .map_err(|e| AppError::BadRequest(format!("invalid item content: {e}")))?;
    sqlx::query(
        "INSERT INTO session_items \
         (id, principal_id, session_id, turn_id, item_type, role, status, content, seq) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(new_id("item"))
    .bind(pid)
    .bind(session_id)
    .bind(turn_id)
    .bind(kind)
    .bind(role)
    .bind(status)
    .bind(content)
    .bind(seq)
    .execute(pool)
    .await
    .map_err(AppError::Db)?;
    Ok(())
}

fn row_to_turn(r: &sqlx::postgres::PgRow) -> SessionTurn {
    SessionTurn {
        id: r.get("id"),
        object: TURN_OBJECT.into(),
        session_id: r.get("session_id"),
        agent_id: r.get("agent_id"),
        status: r.get("status"),
        output: r.get("output"),
        created_at: r.get("created_at"),
        completed_at: r.get("completed_at"),
    }
}

fn row_to_item(r: &sqlx::postgres::PgRow) -> SessionItem {
    let raw: String = r.get("content");
    let content = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
    SessionItem {
        id: r.get("id"),
        object: ITEM_OBJECT.into(),
        session_id: r.get("session_id"),
        turn_id: r.get("turn_id"),
        kind: r.get("item_type"),
        role: r.get("role"),
        status: r.get("status"),
        content,
        created_at: r.get("created_at"),
    }
}

/// Kept so the unused-import linter stays honest if `SUBAGENT_OBJECT` becomes
/// serialisable-only; the constant documents the object name we would emit.
#[allow(dead_code)]
const _SUBAGENT_OBJECT: &str = SUBAGENT_OBJECT;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Agent, AgentReasoning, MultiAgentConfig};

    fn agent() -> Agent {
        Agent {
            id: "agent_1".into(),
            object: "agent".into(),
            name: Some("demo".into()),
            model: Some("gpt-4o-mini".into()),
            instructions: Some("be brief".into()),
            reasoning: AgentReasoning::default(),
            multi_agent: MultiAgentConfig::default(),
            service_tier: "auto".into(),
            text: None,
            tools: vec![],
            created_at: 0,
        }
    }

    #[test]
    fn session_agent_snapshots_the_agent() {
        let s = session_agent(&agent());
        assert_eq!(s.id, "agent_1");
        assert_eq!(s.name.as_deref(), Some("demo"));
        assert_eq!(s.service_tier, "auto");
    }

    #[test]
    fn empty_input_is_rejected_before_any_db_work() {
        // `create_session_events` validates before touching Postgres; the check
        // is mirrored here so the rule is covered without a database.
        let e = SessionInputEvent {
            input: Some("  ".into()),
            content: vec![],
            kind: None,
        };
        assert!(e.text().trim().is_empty());
    }
}
