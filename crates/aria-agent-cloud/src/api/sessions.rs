//! Agent **sessions**: `POST / GET / POST{id} / DELETE /v1/agents/sessions[/{id}]`
//! plus the turn driver (`…/events`, `…/events/stream`) and the read-only
//! `items` / `turns` / `subagents` listings.
//!
//! Handlers only orchestrate; persistence goes through
//! [`crate::store::SessionStore`] so the whole surface is testable without a
//! database.

use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::Json;
use futures::StreamExt;
use serde::Deserialize;
use std::sync::Arc;

use crate::app::{agent_tools, build_agent, build_sse_stream, AppState};
use crate::auth::{Principal, PrincipalKind};
use crate::store::{ItemRow, SessionRow, TurnRow};
use crate::types::{
    Agent, AgentSession, CreateRelationRequest, CreateSessionRequest, GraphRetrieveResponse,
    ListResponse, MemoryItem, PutMemoryRequest, RelationDeleted, RelationGraphQuery, RelationItem,
    RelationListParams, RetrieveTraceWire, ScoredMemoryItem, SessionAgent, SessionDeleted,
    SessionInputEvent, SessionItem, SessionTurn, Subagent, UpdateSessionRequest, ITEM_OBJECT,
    SESSION_OBJECT, TURN_OBJECT,
};
use crate::util::{new_id, now_ms};
use crate::AppError;
use agent_core::context::embed::LocalEmbedder;
use agent_core::context::{
    ContextController, ContextFragment, ContextStore, FragmentKind, GraphRetrieveQuery,
    RecallQuery, Relation, RelationKind, EMBED_DIM,
};

#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

pub fn default_limit() -> i64 {
    20
}

fn session_agent(agent: &Agent) -> SessionAgent {
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

/// Row → wire conversion for a session (needs the agent snapshot).
pub fn to_session(row: &SessionRow, agent: &Agent) -> AgentSession {
    AgentSession {
        id: row.id.clone(),
        object: SESSION_OBJECT.into(),
        agent: session_agent(agent),
        instructions: row.instructions.clone(),
        status: row.status.clone(),
        created_at: row.created_at,
    }
}

pub fn to_turn(row: &TurnRow) -> SessionTurn {
    SessionTurn {
        id: row.id.clone(),
        object: TURN_OBJECT.into(),
        session_id: row.session_id.clone(),
        agent_id: row.agent_id.clone(),
        status: row.status.clone(),
        output: row.output.clone(),
        created_at: row.created_at,
        completed_at: row.completed_at,
    }
}

pub fn to_item(row: &ItemRow) -> SessionItem {
    SessionItem {
        id: row.id.clone(),
        object: ITEM_OBJECT.into(),
        session_id: row.session_id.clone(),
        turn_id: row.turn_id.clone(),
        kind: row.kind.clone(),
        role: row.role.clone(),
        status: row.status.clone(),
        content: row.content.clone(),
        created_at: row.created_at,
    }
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
    let agent = state.agents.get_agent(&principal, &agent_ref).await?;

    let row = SessionRow {
        id: new_id("sess"),
        principal_id: principal.id.clone(),
        agent_id: agent.id.clone(),
        instructions: body.instructions.clone(),
        status: "idle".into(),
        created_at: now_ms(),
    };
    state.sessions.insert_session(&principal, &row).await?;
    Ok(Json(to_session(&row, &agent)))
}

pub async fn list_sessions(
    State(state): State<AppState>,
    principal: Principal,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<AgentSession>>, AppError> {
    let limit = params.limit.clamp(1, 100);
    let rows = state.sessions.list_sessions(&principal, limit).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        // A session whose agent was deleted still lists, with a stub snapshot.
        let agent = match state.agents.get_agent(&principal, &row.agent_id).await {
            Ok(a) => a,
            Err(_) => continue,
        };
        out.push(to_session(row, &agent));
    }
    Ok(Json(ListResponse::new(out)))
}

pub async fn get_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<AgentSession>, AppError> {
    let row = state.sessions.get_session(&principal, &session_id).await?;
    let agent = state.agents.get_agent(&principal, &row.agent_id).await?;
    Ok(Json(to_session(&row, &agent)))
}

pub async fn update_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Json(body): Json<UpdateSessionRequest>,
) -> Result<Json<AgentSession>, AppError> {
    let row = state.sessions.get_session(&principal, &session_id).await?;
    if body.instructions.is_some() {
        state
            .sessions
            .set_instructions(&session_id, body.instructions.as_deref())
            .await?;
    }
    let agent = state.agents.get_agent(&principal, &row.agent_id).await?;
    let mut session = to_session(&row, &agent);
    session.instructions = body.instructions.or(row.instructions);
    Ok(Json(session))
}

pub async fn delete_session(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<SessionDeleted>, AppError> {
    let row = state.sessions.get_session(&principal, &session_id).await?;
    state.sessions.delete_session(&row.id).await?;
    Ok(Json(SessionDeleted {
        id: row.id,
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

    let agent_rt = build_agent(
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
    state
        .sessions
        .finish_turn(&turn_id, "completed", Some(&output))
        .await?;

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
/// events as they happen (SSE). Terminal event is `agent.turn.completed` /
/// `agent.turn.failed`; there is no `[DONE]` sentinel.
pub async fn stream_session_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    principal: Principal,
    Json(body): Json<SessionInputEvent>,
) -> Result<Response, AppError> {
    let input = body.text();
    if input.trim().is_empty() {
        return Err(AppError::BadRequest("input is empty".into()));
    }
    let (agent, instructions) =
        session_agent_and_instructions(&state, &principal, &session_id).await?;
    let turn_id = new_id("turn");
    insert_turn(&state, &principal, &turn_id, &session_id, &agent.id).await?;

    let agent_name = agent.name.clone().unwrap_or_else(|| agent.id.clone());
    let agent_rt = build_agent(&state, &agent_name, &instructions, &session_id, &principal).await?;
    let tools = agent_tools();
    let event_stream = agent_rt
        .run_event_stream(&input, &tools)
        .await
        .map_err(AppError::Core)?;

    let pid = principal.id.clone();
    let session_id_inner = session_id.clone();
    let turn_id_inner = turn_id.clone();
    let agent_id = agent.id.clone();

    // Snapshot the multi-relational memory summary before streaming starts: it
    // rides on the terminal `agent.turn.completed` frame (Jev-Mem
    // `memory_relations`), which is emitted the moment the model reports `Done`,
    // so it must already be attached by then.
    let relation_summary: Vec<serde_json::Value> = match state
        .context_for(&principal)
        .get_relations(&session_id, None, None, None, 20)
        .await
    {
        Ok(rels) => rels
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "session": r.session,
                    "from_id": r.from_id,
                    "to_id": r.to_id,
                    "kind": r.kind.as_str(),
                    "score": r.score,
                    "provenance": r.provenance,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    let wrapped = async_stream::stream! {
        let mut envelope =
            crate::event_envelope::EventEnvelope::new(&turn_id_inner, &agent_id, &session_id_inner);
        envelope.set_memory_relations(relation_summary);
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
                            let _ = state.sessions.insert_item(&ItemRow {
                                id: new_id("item"),
                                principal_id: pid.clone(),
                                session_id: session_id_inner.clone(),
                                turn_id: turn_id_inner.clone(),
                                kind: "function_call".into(),
                                role: None,
                                status: "completed".into(),
                                content,
                                seq,
                                created_at: now_ms(),
                            }).await;
                        }
                        _ => {}
                    }
                    for frame in envelope.push(&ev) {
                        yield frame;
                    }
                }
                Err(e) => {
                    let _ = state
                        .sessions
                        .finish_turn(&turn_id_inner, "failed", Some(&e.to_string()))
                        .await;
                    let _ = state.sessions.insert_item(&ItemRow {
                        id: new_id("item"),
                        principal_id: pid.clone(),
                        session_id: session_id_inner.clone(),
                        turn_id: turn_id_inner.clone(),
                        kind: "message".into(),
                        role: Some("assistant".into()),
                        status: "incomplete".into(),
                        content: serde_json::Value::String(collected.clone()),
                        seq: seq + 1,
                        created_at: now_ms(),
                    }).await;
                    yield envelope.failed(&e.to_string());
                    return;
                }
            }
        }

        for frame in envelope.ensure_closed() {
            yield frame;
        }

        seq += 1;
        let _ = state.sessions.insert_item(&ItemRow {
            id: new_id("item"),
            principal_id: pid.clone(),
            session_id: session_id_inner.clone(),
            turn_id: turn_id_inner.clone(),
            kind: "message".into(),
            role: Some("assistant".into()),
            status: "completed".into(),
            content: serde_json::Value::String(collected.clone()),
            seq,
            created_at: now_ms(),
        }).await;
        let _ = state
            .sessions
            .finish_turn(&turn_id_inner, "completed", Some(&collected))
            .await;
    };

    Ok(build_sse_stream(wrapped))
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
    state.sessions.get_session(&principal, &session_id).await?;
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
    // Write→connect: infer multi-relational edges (Jev-Mem) for the new fragment.
    // Failure here must not fail the memory write itself.
    relate_on_memorize(&store, &frag).await;
    Ok(Json(MemoryItem {
        id: frag.id,
        key: frag.key,
        kind: kind.as_str().to_string(),
        content: frag.content,
        created_at: frag.created_at,
    }))
}

/// After a memory fragment is persisted, infer four-view relations to existing
/// fragments (Jev-Mem write→connect). Errors are logged and swallowed so they
/// never break the primary memory write.
async fn relate_on_memorize(store: &Arc<dyn ContextStore>, frag: &ContextFragment) {
    let ctrl =
        ContextController::with_defaults(store.clone(), Arc::new(LocalEmbedder::new(EMBED_DIM)));
    match ctrl.connect(frag).await {
        Ok(n) => tracing::debug!("memory write→connect inferred {n} relation edge(s)"),
        Err(e) => tracing::warn!("memory write→connect failed (ignored): {e}"),
    }
}

/// `GET /v1/agents/sessions/{id}/memory/{key}` — read a keyed memory.
pub async fn get_session_memory(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, key)): Path<(String, String)>,
) -> Result<Json<MemoryItem>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    let store = state.context_for(&principal);
    let frag = store
        .get_by_key(&session_id, &key)
        .await
        .map_err(|e| AppError::BadRequest(format!("memory read failed: {e}")))?;
    match frag {
        Some(f) => Ok(Json(to_memory_item(f))),
        None => Err(AppError::NotFound),
    }
}

#[derive(Debug, Deserialize)]
pub struct MemoryQuery {
    /// Semantic / keyword query. Omitted (or empty) lists the session's memory.
    pub text: Option<String>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

pub fn default_top_k() -> usize {
    20
}

/// `GET /v1/agents/sessions/{id}/memory?text=…&top_k=…` — recall memory.
///
/// With `text` the store ranks by similarity + keyword score; without it the
/// endpoint lists the session's fragments (oldest first).
pub async fn search_session_memory(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Query(params): Query<MemoryQuery>,
) -> Result<Json<ListResponse<MemoryItem>>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
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

pub fn to_memory_item(f: ContextFragment) -> MemoryItem {
    MemoryItem {
        id: f.id,
        key: f.key,
        kind: f.kind.as_str().to_string(),
        content: f.content,
        created_at: f.created_at,
    }
}

// ------------------------------------------------------- multi-relational memory

pub fn parse_relation_kind(s: &str) -> Result<RelationKind, AppError> {
    s.parse::<RelationKind>()
        .map_err(|e: String| AppError::BadRequest(format!("bad relation_kind: {e}")))
}

fn relation_filter_kind(kind: Option<&str>) -> Result<Option<RelationKind>, AppError> {
    match kind.filter(|k| !k.is_empty()) {
        Some(k) => Ok(Some(parse_relation_kind(k)?)),
        None => Ok(None),
    }
}

/// `POST /v1/agents/sessions/{id}/memory/relations` — create a relation edge.
pub async fn create_session_memory_relation(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Json(body): Json<CreateRelationRequest>,
) -> Result<Json<RelationItem>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    let kind = parse_relation_kind(&body.kind)?;
    let score = body.score.unwrap_or(0.8);
    let provenance = body
        .provenance
        .clone()
        .unwrap_or_else(|| "rest".to_string());
    let created_at = now_ms();
    let rel = Relation {
        session: session_id.clone(),
        from_id: body.from_id.clone(),
        to_id: body.to_id.clone(),
        kind,
        score,
        provenance: provenance.clone(),
        created_at,
    };
    let store = state.context_for(&principal);
    store
        .relate(rel)
        .await
        .map_err(|e| AppError::BadRequest(format!("relate failed: {e}")))?;
    Ok(Json(RelationItem {
        session: session_id,
        from_id: body.from_id,
        to_id: body.to_id,
        kind: body.kind,
        score,
        provenance,
        created_at,
    }))
}

/// `GET /v1/agents/sessions/{id}/memory/relations` — list relation edges.
pub async fn list_session_memory_relations(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Query(params): Query<RelationListParams>,
) -> Result<Json<ListResponse<RelationItem>>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    let kind = relation_filter_kind(params.kind.as_deref())?;
    let top_k = params.top_k.unwrap_or(50).clamp(1, 200);
    let store = state.context_for(&principal);
    let rels = store
        .get_relations(
            &session_id,
            params.from.as_deref(),
            params.to.as_deref(),
            kind,
            top_k,
        )
        .await
        .map_err(|e| AppError::BadRequest(format!("get relations failed: {e}")))?;
    let items = rels
        .into_iter()
        .map(|r| RelationItem {
            session: r.session,
            from_id: r.from_id,
            to_id: r.to_id,
            kind: r.kind.as_str().to_string(),
            score: r.score,
            provenance: r.provenance,
            created_at: r.created_at,
        })
        .collect();
    Ok(Json(ListResponse::new(items)))
}

/// `DELETE /v1/agents/sessions/{id}/memory/relations` — delete relation edges.
pub async fn delete_session_memory_relations(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Query(params): Query<RelationListParams>,
) -> Result<Json<RelationDeleted>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    let kind = relation_filter_kind(params.kind.as_deref())?;
    if params.from.is_none() && params.to.is_none() && kind.is_none() {
        return Err(AppError::BadRequest(
            "relation delete needs at least one of from/to/kind".into(),
        ));
    }
    let store = state.context_for(&principal);
    let deleted = store
        .delete_relations(
            &session_id,
            params.from.as_deref(),
            params.to.as_deref(),
            kind,
        )
        .await
        .map_err(|e| AppError::BadRequest(format!("delete relations failed: {e}")))?;
    Ok(Json(RelationDeleted {
        object: "relation.deleted",
        session: session_id,
        deleted,
    }))
}

/// `GET /v1/agents/sessions/{id}/memory/relations/graph` — bounded graph retrieval.
pub async fn graph_session_memory_relations(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
    Query(params): Query<RelationGraphQuery>,
) -> Result<Json<GraphRetrieveResponse>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    let seeds: Vec<String> = params
        .seeds
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if seeds.is_empty() {
        return Err(AppError::BadRequest("graph query needs >= 1 seed".into()));
    }
    let views: Vec<RelationKind> = match params.views.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => s
            .split(',')
            .map(parse_relation_kind)
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    let q = GraphRetrieveQuery {
        session: session_id.clone(),
        seeds,
        views,
        budget: params.budget.unwrap_or(60),
        max_hops: params.max_hops.unwrap_or(3),
        top_k: params.top_k.unwrap_or(20),
    };
    let store = state.context_for(&principal);
    let res = store
        .expand(&q)
        .await
        .map_err(|e| AppError::BadRequest(format!("graph expand failed: {e}")))?;
    let items = res
        .items
        .into_iter()
        .map(|sf| ScoredMemoryItem {
            id: sf.fragment.id,
            key: sf.fragment.key,
            kind: sf.fragment.kind.as_str().to_string(),
            content: sf.fragment.content,
            created_at: sf.fragment.created_at,
            score: sf.score,
        })
        .collect();
    let trace = RetrieveTraceWire {
        views: res
            .trace
            .views
            .iter()
            .map(|v| v.as_str().to_string())
            .collect(),
        budget: res.trace.budget,
        stop_reason: res.trace.stop_reason,
        hits: res.trace.hits,
    };
    Ok(Json(GraphRetrieveResponse { items, trace }))
}

// ------------------------------------------------------------- read-only reads

pub async fn list_session_items(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<ListResponse<SessionItem>>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    let rows = state.sessions.list_items(&principal, &session_id).await?;
    Ok(Json(ListResponse::new(rows.iter().map(to_item).collect())))
}

pub async fn list_session_turns(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<ListResponse<SessionTurn>>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    let rows = state.sessions.list_turns(&principal, &session_id).await?;
    Ok(Json(ListResponse::new(rows.iter().map(to_turn).collect())))
}

pub async fn get_session_turn(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, turn_id)): Path<(String, String)>,
) -> Result<Json<SessionTurn>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    state
        .sessions
        .get_turn(&principal, &session_id, &turn_id)
        .await?
        .map(|row| Json(to_turn(&row)))
        .ok_or(AppError::NotFound)
}

/// This runtime runs a single agent loop, so no subagents are ever spawned.
/// The endpoint exists so clients can enumerate them and simply get none.
pub async fn list_subagents(
    State(state): State<AppState>,
    principal: Principal,
    Path(session_id): Path<String>,
) -> Result<Json<ListResponse<Subagent>>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    Ok(Json(ListResponse::new(Vec::new())))
}

pub async fn get_subagent(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, _subagent_id)): Path<(String, String)>,
) -> Result<Json<Subagent>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    Err(AppError::NotFound)
}

pub async fn list_subagent_items(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, _subagent_id)): Path<(String, String)>,
) -> Result<Json<ListResponse<SessionItem>>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    Ok(Json(ListResponse::new(Vec::new())))
}

pub async fn list_subagent_turns(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, _subagent_id)): Path<(String, String)>,
) -> Result<Json<ListResponse<SessionTurn>>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    Ok(Json(ListResponse::new(Vec::new())))
}

pub async fn get_subagent_turn(
    State(state): State<AppState>,
    principal: Principal,
    Path((session_id, _subagent_id, _turn_id)): Path<(String, String, String)>,
) -> Result<Json<SessionTurn>, AppError> {
    state.sessions.get_session(&principal, &session_id).await?;
    Err(AppError::NotFound)
}

// ------------------------------------------------------------------- internals

async fn session_agent_and_instructions(
    state: &AppState,
    principal: &Principal,
    session_id: &str,
) -> Result<(Agent, String), AppError> {
    let row = state.sessions.get_session(principal, session_id).await?;
    let agent = state.agents.get_agent(principal, &row.agent_id).await?;
    let mut instructions = agent.instructions.clone().unwrap_or_default();
    if let Some(extra) = row.instructions {
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

async fn insert_turn(
    state: &AppState,
    principal: &Principal,
    turn_id: &str,
    session_id: &str,
    agent_id: &str,
) -> Result<(), AppError> {
    state
        .sessions
        .insert_turn(&TurnRow {
            id: turn_id.to_string(),
            principal_id: principal.id.clone(),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            status: "in_progress".into(),
            output: None,
            created_at: now_ms(),
            completed_at: None,
        })
        .await
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
    state
        .sessions
        .insert_item(&ItemRow {
            id: new_id("item"),
            principal_id: principal.id.clone(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            kind: kind.to_string(),
            role: role.map(str::to_string),
            status: status.to_string(),
            content: content.clone(),
            seq,
            created_at: now_ms(),
        })
        .await
}

/// The tenant filter is applied by the store; this helper exists so tests (and
/// future handlers) can assert the invariant explicitly.
pub fn tenant_scoped(principal: &Principal) -> bool {
    principal.kind != PrincipalKind::Admin
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentReasoning, MultiAgentConfig};

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

    fn session_row() -> SessionRow {
        SessionRow {
            id: "sess_1".into(),
            principal_id: "tenantA".into(),
            agent_id: "agent_1".into(),
            instructions: Some("extra".into()),
            status: "idle".into(),
            created_at: 42,
        }
    }

    #[test]
    fn session_agent_snapshots_the_agent() {
        let s = to_session(&session_row(), &agent());
        assert_eq!(s.id, "sess_1");
        assert_eq!(s.agent.id, "agent_1");
        assert_eq!(s.agent.name.as_deref(), Some("demo"));
        assert_eq!(s.agent.service_tier, "auto");
        assert_eq!(s.instructions.as_deref(), Some("extra"));
        assert_eq!(s.created_at, 42);
    }

    #[test]
    fn turn_and_item_roundtrip_wire_shape() {
        let turn = to_turn(&TurnRow {
            id: "turn_1".into(),
            principal_id: "tenantA".into(),
            session_id: "sess_1".into(),
            agent_id: "agent_1".into(),
            status: "completed".into(),
            output: Some("hi".into()),
            created_at: 1,
            completed_at: Some(2),
        });
        assert_eq!(turn.object, TURN_OBJECT);
        assert_eq!(turn.output.as_deref(), Some("hi"));

        let item = to_item(&ItemRow {
            id: "item_1".into(),
            principal_id: "tenantA".into(),
            session_id: "sess_1".into(),
            turn_id: "turn_1".into(),
            kind: "message".into(),
            role: Some("user".into()),
            status: "completed".into(),
            content: serde_json::json!("hello"),
            seq: 0,
            created_at: 1,
        });
        assert_eq!(item.object, ITEM_OBJECT);
        assert_eq!(item.kind, "message");
    }

    #[test]
    fn relation_kind_parsing_rejects_unknown_views() {
        assert_eq!(
            parse_relation_kind("semantic").unwrap(),
            RelationKind::Semantic
        );
        assert!(parse_relation_kind("bogus").is_err());
        assert_eq!(relation_filter_kind(Some("")).unwrap(), None);
        assert_eq!(relation_filter_kind(None).unwrap(), None);
        assert!(relation_filter_kind(Some("nope")).is_err());
    }

    #[test]
    fn tenant_scoped_excludes_admins() {
        assert!(tenant_scoped(&Principal {
            id: "a".into(),
            kind: PrincipalKind::User,
            scopes: crate::auth::ApiKeyScopes::read_write(),
        }));
        assert!(!tenant_scoped(&Principal::admin()));
    }

    #[test]
    fn default_limits() {
        assert_eq!(default_limit(), 20);
        assert_eq!(default_top_k(), 20);
    }
}
