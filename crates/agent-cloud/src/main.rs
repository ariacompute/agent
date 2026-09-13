//! `agent-cloud` — the Agents Cloud API.
//!
//! * axum HTTP server.
//! * Postgres is used **only** for structured metadata (`agents`, `runs`).
//! * Conversational context is injected from `agent-memo` (local/embedded
//!   store) — never persisted in Postgres.
//! * Model calls go to the OpenAI Responses/Chat API via `agent-core`'s
//!   `OpenAiModel` (enabled by the `openai` feature).

use std::convert::Infallible;
use std::sync::Arc;

use agent_core::{Agent, AgentConfig, CoreError, OpenAiModel};
use agent_memo::{MemoStore, SledMemoStore};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::{sse::Sse, IntoResponse, Response};
use axum::extract::Request;
use axum::middleware::{from_fn_with_state, Next};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::Executor;
use sqlx::Row;

const DEFAULT_MODEL: &str = "gpt-4o-mini";

#[derive(Clone)]
struct AppState {
    pool: sqlx::PgPool,
    /// Shared memo store — the single source of conversational context.
    memo: Arc<dyn MemoStore>,
}

#[derive(Serialize, Deserialize)]
struct AgentRecord {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct CreateAgentBody {
    name: String,
}

#[derive(Deserialize)]
struct RunBody {
    agent_id: String,
    session: String,
    input: String,
}

#[derive(Serialize)]
struct RunRecord {
    id: String,
    agent_id: String,
    session: String,
    output: String,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug)]
enum AppError {
    Db(sqlx::Error),
    Core(agent_core::CoreError),
    NotFound,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AppError::Db(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
            AppError::Core(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("core: {e}")),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
        };
        (status, Json(ErrorBody { error: msg })).into_response()
    }
}

/// One streamed token, serialized into an SSE `data:` frame.
#[derive(Serialize)]
struct StreamToken {
    token: String,
}

/// Pure auth check used by [`require_auth`].
///
/// Returns `true` when the request is allowed: always allowed when `expected`
/// is `None` (open mode, i.e. `AGENT_CLOUD_API_KEY` is unset); otherwise the
/// `presented` `Authorization` header must equal `expected` as either
/// `Bearer <key>` or `ApiKey <key>`.
fn auth_ok(expected: Option<&str>, presented: &str) -> bool {
    match expected {
        None => true,
        Some(k) => presented
            .strip_prefix("Bearer ")
            .or_else(|| presented.strip_prefix("ApiKey "))
            .map(|tok| tok.trim() == k)
            .unwrap_or(false),
    }
}

/// Bearer / ApiKey auth gate.
///
/// Reads `AGENT_CLOUD_API_KEY` from the environment. When it is set (non-empty)
/// every request must carry a matching `Authorization: Bearer <key>` or
/// `Authorization: ApiKey <key>` header, otherwise `401` is returned. When the
/// env var is absent the service runs open (dev convenience).
async fn require_auth(State(_state): State<AppState>, req: Request, next: Next) -> Result<Response, StatusCode> {
    let expected = match std::env::var("AGENT_CLOUD_API_KEY") {
        Ok(k) if !k.is_empty() => Some(k),
        _ => return Ok(next.run(req).await),
    };
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !auth_ok(expected.as_deref(), presented) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

/// Build an [`Agent`] for the given run body (resolves the agent name from the
/// Postgres metadata store and wires the OpenAI model + memo context).
async fn build_agent(state: &AppState, body: &RunBody) -> Result<Agent, AppError> {
    let name = agent_name(state, &body.agent_id).await?;
    let cfg = agent_config(&name, &body.session);
    Agent::with_model(
        cfg,
        Box::new(OpenAiModel::new(DEFAULT_MODEL)),
        state.memo.clone(),
    )
    .map_err(AppError::Core)
}

async fn create_agent(
    State(state): State<AppState>,
    Json(body): Json<CreateAgentBody>,
) -> Result<Json<AgentRecord>, AppError> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind(&id)
        .bind(&body.name)
        .execute(&state.pool)
        .await
        .map_err(AppError::Db)?;
    Ok(Json(AgentRecord {
        id,
        name: body.name,
    }))
}

async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentRecord>, AppError> {
    let row = sqlx::query("SELECT id, name FROM agents WHERE id = $1")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Db)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(AgentRecord {
        id: row.get("id"),
        name: row.get("name"),
    }))
}

async fn run_agent(
    State(state): State<AppState>,
    Json(body): Json<RunBody>,
) -> Result<Json<RunRecord>, AppError> {
    let agent = build_agent(&state, &body).await?;
    let output = agent.run(&body.input).await.map_err(AppError::Core)?;
    let run_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO runs (id, agent_id, session, input, output) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&run_id)
    .bind(&body.agent_id)
    .bind(&body.session)
    .bind(&body.input)
    .bind(&output)
    .execute(&state.pool)
    .await
    .map_err(AppError::Db)?;
    Ok(Json(RunRecord {
        id: run_id,
        agent_id: body.agent_id,
        session: body.session,
        output,
    }))
}

async fn run_agent_stream(
    State(state): State<AppState>,
    Json(body): Json<RunBody>,
) -> Sse<BoxStream<'static, Result<Event, Infallible>>> {
    // Build the agent (resolves name + memo) before streaming.
    let built = build_agent(&state, &body).await;
    let token_stream = match built {
        Ok(agent) => agent.run_stream(&body.input).await,
        Err(e) => Err(CoreError::Model(match e {
            AppError::Db(d) => format!("db: {d}"),
            AppError::Core(c) => format!("core: {c}"),
            AppError::NotFound => "agent not found".to_string(),
        })),
    };

    match token_stream {
        Ok(tokens) => {
            // Map each model token to an SSE frame; terminate with `[DONE]`.
            let sse = tokens
                .map(|res| {
                    Ok::<Event, Infallible>(match res {
                        Ok(token) => Event::default()
                            .json_data(StreamToken { token })
                            .unwrap_or_else(|_| Event::default().data("[encode-error]")),
                        Err(e) => Event::default().data(format!("error: {e}")),
                    })
                })
                .chain(stream::once(async { Ok(Event::default().data("[DONE]")) }));
            Sse::new(Box::pin(sse))
        }
        Err(e) => Sse::new(Box::pin(stream::iter(vec![Ok(
            Event::default().data(format!("error: {e}"))
        )]))),
    }
}

async fn agent_name(state: &AppState, agent_id: &str) -> Result<String, AppError> {
    let row = sqlx::query("SELECT name FROM agents WHERE id = $1")
        .bind(agent_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Db)?
        .ok_or(AppError::NotFound)?;
    Ok(row.get("name"))
}

fn agent_config(name: &str, session: &str) -> AgentConfig {
    AgentConfig {
        session: session.to_string(),
        agent_name: name.to_string(),
        sandbox_provider: "docker".into(),
        model: DEFAULT_MODEL.into(),
    }
}

async fn ensure_schema(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
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
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/agent".into());
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await?;
    ensure_schema(&pool).await?;

    // Local/embedded memo store. This is the ONLY holder of conversational context.
    let memo: Arc<dyn MemoStore> = SledMemoStore::memory().map_err(|e| e.to_string())?;
    let state = AppState { pool, memo };

    let app = Router::new()
        .route("/v1/agents", post(create_agent))
        .route("/v1/agents/:id", get(get_agent))
        .route("/v1/runs", post(run_agent))
        .route("/v1/runs/stream", post(run_agent_stream))
        .with_state(state.clone())
        // Auth gate (Bearer / ApiKey); open when AGENT_CLOUD_API_KEY is unset.
        // `from_fn_with_state` threads `AppState` into the middleware closure.
        .layer(from_fn_with_state(state, require_auth));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 3000));
    tracing::info!("agent-cloud listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_open_without_key() {
        // Simulated open mode (no expected key): every request is allowed.
        assert!(auth_ok(None, ""));
        assert!(auth_ok(None, "Bearer anything"));
        assert!(auth_ok(None, "ApiKey anything"));
    }

    #[test]
    fn auth_rejects_missing_or_wrong_header() {
        let key = Some("secret");
        assert!(!auth_ok(key, ""));
        assert!(!auth_ok(key, "Basic secret"));
        assert!(!auth_ok(key, "Bearer wrong"));
        assert!(!auth_ok(key, "ApiKey wrong"));
        assert!(!auth_ok(key, "secret")); // no scheme prefix
    }

    #[test]
    fn auth_accepts_bearer_and_apikey() {
        let key = Some("secret");
        assert!(auth_ok(key, "Bearer secret"));
        assert!(auth_ok(key, "ApiKey secret"));
        assert!(auth_ok(key, "Bearer  secret")); // internal whitespace is trimmed
        assert!(!auth_ok(key, "Bearer secretx"));
        assert!(!auth_ok(key, "Bearer secrets"));
    }

    #[test]
    fn agent_config_defaults() {
        let cfg = agent_config("my-agent", "sess-1");
        assert_eq!(cfg.agent_name, "my-agent");
        assert_eq!(cfg.session, "sess-1");
        assert_eq!(cfg.sandbox_provider, "docker");
        assert_eq!(cfg.model, DEFAULT_MODEL);
    }

    #[test]
    fn error_body_serializes() {
        let body = ErrorBody {
            error: "boom".into(),
        };
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains("boom"));
    }
}
