//! The JSON-RPC dispatcher in front of [`super::executor`].
//!
//! Two rules shape the HTTP status codes, and both come from the first client
//! (agenthub's `adapter/agent.go`):
//!
//! * a **malformed JSON-RPC envelope** is a transport-level failure and answers
//!   `400`;
//! * everything else — including `-32602 invalid params`, `-32001 task_not_found`
//!   and `-32002 task_not_cancelable` — answers `200` with a JSON-RPC `error`
//!   object, because that client classifies any non-2xx as
//!   `resource_unreachable` and would never see the code.
//!
//! Authentication and scopes are not re-implemented here: the router sits behind
//! [`crate::auth::require_auth`], and the dispatcher only checks that a mutating
//! method presented a key with `write` (a read-only key gets a real `403`, which
//! is about the caller, not about the call).

use std::convert::Infallible;

use aria_agent_a2a::jsonrpc::{code, Method, Request, Response, RpcError};
use aria_agent_a2a::types::{
    MessageSendParams, PushGetParams, PushSetParams, StreamFrame, TaskQueryParams, TaskResumeParams,
};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response as HttpResponse};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::app::AppState;
use crate::auth::Principal;
use crate::types::Agent;
use crate::AppError;

use super::executor::{self, StartError, StartRequest, TaskFailure};
use super::hitl::{parse_decision, ResumeInput};
use super::push::validate_push_url;

/// `POST /a2a` — the deployment's default agent.
pub async fn handle_default(
    State(state): State<AppState>,
    principal: Principal,
    body: Bytes,
) -> HttpResponse {
    let agent_id = state.a2a.config.default_agent.clone();
    dispatch(state, principal, agent_id, body).await
}

/// `POST /v1/agents/:agent_id/a2a` — one named agent.
pub async fn handle_agent(
    State(state): State<AppState>,
    principal: Principal,
    Path(agent_id): Path<String>,
    body: Bytes,
) -> HttpResponse {
    dispatch(state, principal, Some(agent_id), body).await
}

/// Parse the envelope, check the scope and hand the method its params.
async fn dispatch(
    state: AppState,
    principal: Principal,
    agent_id: Option<String>,
    body: Bytes,
) -> HttpResponse {
    let request = match serde_json::from_slice::<Request>(&body) {
        Ok(request) => request,
        Err(e) => {
            // Invalid JSON is `-32700`; valid JSON that is not an envelope is
            // `-32600`. Both are transport-level, so both are `400`.
            let code = match serde_json::from_slice::<Value>(&body) {
                Ok(_) => code::INVALID_REQUEST,
                Err(_) => code::PARSE_ERROR,
            };
            return bad_envelope(code, format!("invalid JSON-RPC request: {e}"));
        }
    };
    let id = request.id.clone();
    let Some(method) = Method::parse(&request.method) else {
        return method_error(
            id,
            code::METHOD_NOT_FOUND,
            format!("unknown method `{}`", request.method),
        );
    };
    let allowed = if method.needs_write() {
        principal.scopes.write
    } else {
        principal.scopes.read
    };
    if !allowed {
        let scope = if method.needs_write() {
            "write"
        } else {
            "read"
        };
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": {
                    "code": "insufficient_scope",
                    "message": format!("`{}` requires the `{scope}` scope", method.name()),
                }
            })),
        )
            .into_response();
    }
    #[rustfmt::skip]
    tracing::debug!(principal = principal.id.as_str(), method = method.name(), "a2a request");

    match method {
        Method::MessageSend => {
            message_send(&state, &principal, &agent_id, id, request.params).await
        }
        Method::MessageStream => {
            message_stream(&state, &principal, &agent_id, id, request.params).await
        }
        Method::TasksGet => tasks_get(&state, &principal, id, request.params).await,
        Method::TasksCancel => tasks_cancel(&state, &principal, id, request.params).await,
        Method::TasksResume => tasks_resume(&state, &principal, id, request.params).await,
        Method::PushSet => push_set(&state, &principal, id, request.params).await,
        Method::PushGet => push_get(&state, &principal, id, request.params).await,
    }
}

// --- message/send -------------------------------------------------------

async fn message_send(
    state: &AppState,
    principal: &Principal,
    agent_id: &Option<String>,
    id: Option<Value>,
    params: Value,
) -> HttpResponse {
    let params = match serde_json::from_value::<MessageSendParams>(params) {
        Ok(params) => params,
        Err(e) => {
            return method_error(id, code::INVALID_PARAMS, format!("params.message: {e}"));
        }
    };
    let agent = match resolve_agent(state, principal, agent_id).await {
        Ok(agent) => agent,
        Err(e) => return method_error(id, e.code, e.message),
    };
    let push = params
        .configuration
        .as_ref()
        .and_then(|cfg| cfg.push_notification.clone());
    if let Some(config) = &push {
        if let Err(reason) = validate_push_url(&config.url) {
            return method_error(id, code::INVALID_PARAMS, reason);
        }
    }
    let started = match executor::start(
        state,
        StartRequest {
            principal: principal.clone(),
            agent,
            message: params.message,
            push,
        },
    )
    .await
    {
        Ok(started) => started,
        Err(e) => return start_error(id, e),
    };
    let mut frames = started.frames;
    settle(&mut frames).await;
    let task = match state.a2a.tasks.get(principal, &started.task_id).await {
        Ok(row) => {
            let history =
                executor::load_history(&state.a2a.tasks, principal, &started.task_id, None).await;
            executor::task_from(&row, Some(history))
        }
        // The run is over but the row is gone (a store that lost it): report what
        // was accepted instead of an internal error.
        Err(_) => started.task,
    };
    ok(id, json!(task))
}

// --- message/stream -----------------------------------------------------

async fn message_stream(
    state: &AppState,
    principal: &Principal,
    agent_id: &Option<String>,
    id: Option<Value>,
    params: Value,
) -> HttpResponse {
    let params = match serde_json::from_value::<MessageSendParams>(params) {
        Ok(params) => params,
        Err(e) => {
            return method_error(id, code::INVALID_PARAMS, format!("params.message: {e}"));
        }
    };
    let agent = match resolve_agent(state, principal, agent_id).await {
        Ok(agent) => agent,
        Err(e) => return method_error(id, e.code, e.message),
    };
    let push = params
        .configuration
        .as_ref()
        .and_then(|cfg| cfg.push_notification.clone());
    if let Some(config) = &push {
        if let Err(reason) = validate_push_url(&config.url) {
            return method_error(id, code::INVALID_PARAMS, reason);
        }
    }
    let started = match executor::start(
        state,
        StartRequest {
            principal: principal.clone(),
            agent,
            message: params.message,
            push,
        },
    )
    .await
    {
        Ok(started) => started,
        Err(e) => return start_error(id, e),
    };
    // The task object opens the stream; every later frame is a delta. There is
    // no sentinel — the `final: true` frame is the end.
    let first = StreamFrame::Task(started.task);
    let stream = futures::stream::unfold(
        (Some(first), started.frames, id, false),
        |(pending, mut frames, id, done)| async move {
            let frame = match pending {
                Some(frame) => frame,
                None => recv_frame(&mut frames).await?,
            };
            let last = done || frame.is_final();
            let event = sse_event(&frame, &id);
            Some((Ok::<Event, Infallible>(event), (None, frames, id, last)))
        },
    );
    Sse::new(stream).into_response()
}

// --- tasks/get|cancel|resume --------------------------------------------

async fn tasks_get(
    state: &AppState,
    principal: &Principal,
    id: Option<Value>,
    params: Value,
) -> HttpResponse {
    let params = match serde_json::from_value::<TaskQueryParams>(params) {
        Ok(params) => params,
        Err(e) => return method_error(id, code::INVALID_PARAMS, format!("params: {e}")),
    };
    let task_id = params.resolve_id().trim().to_string();
    if task_id.is_empty() {
        return method_error(id, code::INVALID_PARAMS, "tasks/get needs `id`");
    }
    let row = match state.a2a.tasks.get(principal, &task_id).await {
        Ok(row) => row,
        Err(e) => return store_error(id, e),
    };
    let history =
        executor::load_history(&state.a2a.tasks, principal, &task_id, params.history_length).await;
    ok(id, json!(executor::task_from(&row, Some(history))))
}

async fn tasks_cancel(
    state: &AppState,
    principal: &Principal,
    id: Option<Value>,
    params: Value,
) -> HttpResponse {
    let params = match serde_json::from_value::<TaskQueryParams>(params) {
        Ok(params) => params,
        Err(e) => return method_error(id, code::INVALID_PARAMS, format!("params: {e}")),
    };
    let task_id = params.resolve_id().trim().to_string();
    if task_id.is_empty() {
        return method_error(id, code::INVALID_PARAMS, "tasks/cancel needs `id`");
    }
    match executor::cancel(state, principal, &task_id).await {
        Ok(task) => ok(id, json!(task)),
        Err(e) => task_failure(id, e),
    }
}

async fn tasks_resume(
    state: &AppState,
    principal: &Principal,
    id: Option<Value>,
    params: Value,
) -> HttpResponse {
    let params = match serde_json::from_value::<TaskResumeParams>(params) {
        Ok(params) => params,
        Err(e) => return method_error(id, code::INVALID_PARAMS, format!("params: {e}")),
    };
    let task_id = params.resolve_id().trim().to_string();
    if task_id.is_empty() {
        return method_error(id, code::INVALID_PARAMS, "tasks/resume needs `id`");
    }
    let input = match resume_input(&params) {
        Ok(input) => input,
        Err(reason) => return method_error(id, code::INVALID_PARAMS, reason),
    };
    match executor::resume(state, principal, &task_id, input).await {
        Ok(task) => ok(id, json!(task)),
        Err(e) => task_failure(id, e),
    }
}

/// The answer carried by `tasks/resume`: `metadata.decision` first (an
/// approval), then `metadata.answer` (a question), then the message text.
fn resume_input(params: &TaskResumeParams) -> Result<ResumeInput, String> {
    if let Some(metadata) = &params.metadata {
        if let Some(decision) = metadata.get("decision").and_then(Value::as_str) {
            return parse_decision(decision)
                .map(ResumeInput::Decision)
                .ok_or_else(|| {
                    format!("unknown decision `{decision}` (use allow/deny, approve/reject)")
                });
        }
        if let Some(answer) = metadata.get("answer").and_then(Value::as_str) {
            return Ok(ResumeInput::Text(answer.to_string()));
        }
    }
    let text = params
        .message
        .as_ref()
        .map(|message| message.text())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Err("tasks/resume needs a message, metadata.decision or metadata.answer".into());
    }
    // Text stays text even when it reads like a decision: whether "yes" means
    // `allow` or is simply the answer to "does it work?" depends on what is
    // parked, and only the seam knows that.
    Ok(ResumeInput::Text(text))
}

// --- push notifications -------------------------------------------------

async fn push_set(
    state: &AppState,
    principal: &Principal,
    id: Option<Value>,
    params: Value,
) -> HttpResponse {
    let params = match serde_json::from_value::<PushSetParams>(params) {
        Ok(params) => params,
        Err(e) => return method_error(id, code::INVALID_PARAMS, format!("params: {e}")),
    };
    let task_id = params.resolve_id().trim().to_string();
    if task_id.is_empty() {
        return method_error(
            id,
            code::INVALID_PARAMS,
            "tasks/pushNotification/set needs `taskId`",
        );
    }
    let Some(config) = params.config() else {
        return method_error(
            id,
            code::INVALID_PARAMS,
            "tasks/pushNotification/set needs `pushNotificationConfig`",
        );
    };
    if let Err(reason) = validate_push_url(&config.url) {
        return method_error(id, code::INVALID_PARAMS, reason);
    }
    match state.a2a.tasks.set_push(principal, &task_id, config).await {
        Ok(()) => ok(id, json!(config)),
        Err(e) => store_error(id, e),
    }
}

async fn push_get(
    state: &AppState,
    principal: &Principal,
    id: Option<Value>,
    params: Value,
) -> HttpResponse {
    let params = match serde_json::from_value::<PushGetParams>(params) {
        Ok(params) => params,
        Err(e) => return method_error(id, code::INVALID_PARAMS, format!("params: {e}")),
    };
    let task_id = params.resolve_id().trim().to_string();
    if task_id.is_empty() {
        return method_error(
            id,
            code::INVALID_PARAMS,
            "tasks/pushNotification/get needs `taskId`",
        );
    }
    match state.a2a.tasks.get(principal, &task_id).await {
        Ok(row) => match row.push {
            Some(config) => ok(id, json!(config)),
            None => method_error(
                id,
                code::RESOURCE_NOT_FOUND,
                "no push notification is configured for this task",
            ),
        },
        Err(e) => store_error(id, e),
    }
}

// --- GET /a2a/stream ----------------------------------------------------

/// Query for the replay endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct ReplayQuery {
    /// The task to follow.
    #[serde(default)]
    pub task: String,
    /// Only events with a greater sequence number (0 = from the start).
    #[serde(default)]
    pub since: u64,
}

/// `GET /a2a/stream?task=…&since=…`: replay the coarse log, then follow live.
///
/// Frames are **not** wrapped in a JSON-RPC envelope here — this endpoint is the
/// same shape as agenthub's `handleA2AStream`, which reads `event:`/`data:`
/// directly. Subscribe happens before the replay so a frame produced during the
/// read cannot be lost; a frame that appears in both is emitted once.
pub async fn stream(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<ReplayQuery>,
) -> HttpResponse {
    if !principal.scopes.read {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": {
                    "code": "insufficient_scope",
                    "message": "`/a2a/stream` requires the `read` scope",
                }
            })),
        )
            .into_response();
    }
    let task_id = query.task.trim().to_string();
    if task_id.is_empty() {
        return error_body(
            StatusCode::BAD_REQUEST,
            "invalid_params",
            "`task` is required",
        );
    }
    if let Err(e) = state.a2a.tasks.get(&principal, &task_id).await {
        return match e {
            AppError::NotFound => {
                error_body(StatusCode::NOT_FOUND, "task_not_found", "no such task")
            }
            other => {
                tracing::warn!(error = ?other, "a2a replay could not read the task");
                error_body(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "the task could not be read",
                )
            }
        };
    }
    let live = state.a2a.runtime.subscribe(&task_id).await;
    let events = state
        .a2a
        .tasks
        .events_since(&principal, &task_id, query.since, 1000)
        .await
        .unwrap_or_default();
    let mut replayed: Vec<Value> = Vec::with_capacity(events.len());
    let mut history = Vec::new();
    for event in &events {
        let Ok(frame) = serde_json::from_value::<StreamFrame>(event.payload.clone()) else {
            continue;
        };
        replayed.push(event.payload.clone());
        history.push(Ok::<Event, Infallible>(sse_plain(&frame)));
    }
    let Some(receiver) = live else {
        // Nothing is running: the replay is the whole answer.
        return Sse::new(futures::stream::iter(history)).into_response();
    };
    let tail = futures::stream::unfold(
        (receiver, replayed, false),
        |(mut receiver, replayed, done)| async move {
            if done {
                return None;
            }
            loop {
                let frame = recv_frame(&mut receiver).await?;
                // A frame the replay already delivered must not appear twice:
                // artifact chunks are append-only.
                if replayed.contains(&json!(frame)) {
                    continue;
                }
                let last = frame.is_final();
                return Some((
                    Ok::<Event, Infallible>(sse_plain(&frame)),
                    (receiver, replayed, last),
                ));
            }
        },
    );
    Sse::new(futures::stream::iter(history).chain(tail)).into_response()
}

// --- shared helpers -----------------------------------------------------

/// The agent a request addresses, or the `-32004` to answer with.
struct AgentGone {
    code: i64,
    message: String,
}

async fn resolve_agent(
    state: &AppState,
    principal: &Principal,
    agent_id: &Option<String>,
) -> Result<Agent, AgentGone> {
    let missing = |message: &str| AgentGone {
        code: code::AGENT_NOT_FOUND,
        message: message.to_string(),
    };
    if !state.a2a.enabled {
        return Err(missing("A2A is not configured on this deployment"));
    }
    let Some(agent_id) = agent_id.as_deref() else {
        return Err(missing("no default A2A agent is configured"));
    };
    state
        .agents
        .get_agent(principal, agent_id)
        .await
        .map_err(|e| {
            // A broken lookup is not a client error, but the caller must not
            // learn whether an id exists for someone else: log it, answer the
            // same as a miss.
            tracing::warn!(error = ?e, "a2a: the agent could not be resolved");
            missing("no such agent")
        })
}

/// The next frame, skipping a lag (the reader fell behind; the replay log is
/// what recovers the gap). `None` when the sender is gone, which is how a
/// stream ends.
pub(crate) async fn recv_frame(
    frames: &mut broadcast::Receiver<StreamFrame>,
) -> Option<StreamFrame> {
    loop {
        match frames.recv().await {
            Ok(frame) => return Some(frame),
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

/// Wait until the run settles: a terminal frame or `input-required`.
async fn settle(frames: &mut broadcast::Receiver<StreamFrame>) {
    while let Some(frame) = recv_frame(frames).await {
        if frame.is_final() {
            return;
        }
    }
}

fn sse_event(frame: &StreamFrame, id: &Option<Value>) -> Event {
    let data = json!({"jsonrpc": "2.0", "id": id, "result": frame});
    Event::default().event(frame.event()).data(data.to_string())
}

/// The replay endpoint is an HTTP read, not a JSON-RPC call: it frames the
/// stored events as they were broadcast, with no envelope to unwrap.
fn sse_plain(frame: &StreamFrame) -> Event {
    Event::default()
        .event(frame.event())
        .data(serde_json::to_string(frame).unwrap_or_default())
}

fn ok(id: Option<Value>, result: Value) -> HttpResponse {
    Json(Response::ok(id, result)).into_response()
}

/// A method-level error: `200` so the caller reads the code instead of treating
/// the whole endpoint as unreachable.
fn method_error(id: Option<Value>, code: i64, message: impl Into<String>) -> HttpResponse {
    Json(Response::failed(id, RpcError::new(code, message))).into_response()
}

/// An envelope that is not valid JSON-RPC at all: `400`.
fn bad_envelope(code: i64, message: impl Into<String>) -> HttpResponse {
    (
        StatusCode::BAD_REQUEST,
        Json(Response::failed(None, RpcError::new(code, message))),
    )
        .into_response()
}

fn error_body(status: StatusCode, code: &str, message: &str) -> HttpResponse {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

/// Map an [`AppError`] onto the A2A catalogue. Only "not found" and "conflict"
/// are meaningful to a caller; everything else is logged and generic, so a
/// database message never becomes remote output.
fn store_error(id: Option<Value>, error: AppError) -> HttpResponse {
    match error {
        AppError::NotFound => method_error(id, code::TASK_NOT_FOUND, "no such task"),
        AppError::Conflict(message) => method_error(id, code::INVALID_STATE_TRANSITION, message),
        other => {
            tracing::warn!(error = ?other, "a2a store failure");
            method_error(id, code::INTERNAL_ERROR, "the task could not be updated")
        }
    }
}

fn start_error(id: Option<Value>, error: StartError) -> HttpResponse {
    match error {
        StartError::Empty => {
            method_error(id, code::INVALID_PARAMS, "the message carries no text part")
        }
        StartError::ContextBusy => method_error(
            id,
            code::INVALID_STATE_TRANSITION,
            "another task is already running in this context",
        ),
        StartError::Store(e) => store_error(id, e),
    }
}

fn task_failure(id: Option<Value>, failure: TaskFailure) -> HttpResponse {
    match failure {
        TaskFailure::NotFound => method_error(id, code::TASK_NOT_FOUND, "no such task"),
        TaskFailure::NotCancelable => method_error(
            id,
            code::TASK_NOT_CANCELABLE,
            "the task is already in a terminal state",
        ),
        TaskFailure::NotResumable => method_error(
            id,
            code::TASK_NOT_RESUMABLE,
            "the task is not waiting for input",
        ),
        TaskFailure::Mismatch => method_error(
            id,
            code::INVALID_PARAMS,
            "the answer does not match the pending request",
        ),
        TaskFailure::Store(e) => store_error(id, e),
    }
}
