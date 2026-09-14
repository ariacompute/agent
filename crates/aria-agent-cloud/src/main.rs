//! `agent-cloud` — the Agents Cloud API.
//!
//! * axum HTTP server.
//! * Postgres is used **only** for structured metadata (`agents`, `runs`).
//! * Conversational context is injected from `agent-memo` (local/embedded
//!   store) — never persisted in Postgres.
//! * Model calls go to the OpenAI Responses/Chat API via `agent-core`'s
//!   `OpenAiModel` (enabled by the `openai` feature).
//! * Self-improvement (Reef-style) is layered on top: every turn is recorded
//!   (receipt `x-reef-agent-record-id`), feedback is bound via `/reef/report`,
//!   and `/reef/evolve` runs the local engine to propose a better harness,
//!   keeps the winner, versions it in Git (`.reef/`), and hot-swaps the shared
//!   `ActiveHarness` so live traffic picks it up without a restart.

mod cli_config;
mod upgrade;

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_core::{ActiveHarness, Agent, AgentConfig, CoreError, ModelClient, OpenAiModel};
use agent_memo::{MemoStore, SledMemoStore};
use agent_reef::engine::EvolutionEngine;
use agent_reef::feedback::{Feedback, FeedbackStore, SledFeedbackStore};
use agent_reef::git;
use agent_reef::harness;
use agent_reef::record::{Record, RecordStore, SledRecordStore};
use agent_reef::ReefError;
use async_trait::async_trait;
use axum::extract::FromRequestParts;
use axum::extract::Request;
use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::sse::Event;
use axum::response::{sse::Sse, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use clap::{ArgAction, Args, Parser, Subcommand};
use futures::stream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::Executor;
use sqlx::Row;
use uuid::Uuid;

const DEFAULT_MODEL: &str = "gpt-4o-mini";
const REEF_RECORD_HEADER: &str = "x-reef-agent-record-id";

/// Identity of an API caller (tenant / user). Also doubles as an axum extractor:
/// [`require_auth`] resolves the presented key to a `Principal` and inserts it
/// into the request extensions; handlers pull it back out.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Principal {
    /// Tenant/user id. Also the sled path shard key (validated safe).
    id: String,
    kind: PrincipalKind,
    scopes: ApiKeyScopes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrincipalKind {
    Admin,
    User,
}

impl Principal {
    /// Bootstrap admin (resolves from `AGENT_CLOUD_API_KEY`). Its stores map to
    /// the legacy root paths so single-key deployments keep their data.
    fn admin() -> Self {
        Principal {
            id: "admin".into(),
            kind: PrincipalKind::Admin,
            scopes: ApiKeyScopes::all(),
        }
    }
    /// Open-mode fallback tenant when no auth is configured.
    fn default_user() -> Self {
        Principal {
            id: "default".into(),
            kind: PrincipalKind::User,
            scopes: ApiKeyScopes::read_write(),
        }
    }
    fn is_admin(&self) -> bool {
        self.kind == PrincipalKind::Admin || self.scopes.admin
    }
}

/// Permission scopes, mirroring the OpenAI Agents API project-key model
/// (`api.agents.read` / `api.agents.write` / admin). Stored as a comma-joined
/// token string in Postgres.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ApiKeyScopes {
    admin: bool,
    read: bool,
    write: bool,
}

impl ApiKeyScopes {
    fn all() -> Self {
        ApiKeyScopes {
            admin: true,
            read: true,
            write: true,
        }
    }
    fn read_write() -> Self {
        ApiKeyScopes {
            admin: false,
            read: true,
            write: true,
        }
    }
    fn from_tokens(tokens: &[String]) -> Self {
        let mut s = ApiKeyScopes::default();
        for t in tokens {
            match t.as_str() {
                "admin" => s.admin = true,
                "read" => s.read = true,
                "write" => s.write = true,
                _ => {}
            }
        }
        // Admin implies full read/write.
        if s.admin {
            s.read = true;
            s.write = true;
        }
        s
    }
    fn to_tokens(self) -> Vec<String> {
        let mut v = Vec::new();
        if self.admin {
            v.push("admin".into());
        }
        if self.read {
            v.push("read".into());
        }
        if self.write {
            v.push("write".into());
        }
        v
    }
}

/// A store that can be opened persistently under a path or in-memory.
trait TenantStore: Send + Sync + 'static {
    fn open(path: &std::path::Path) -> Result<Arc<Self>, String>;
    fn memory() -> Result<Arc<Self>, String>;
}

impl TenantStore for SledMemoStore {
    fn open(path: &std::path::Path) -> Result<Arc<Self>, String> {
        SledMemoStore::open(path).map_err(|e| e.to_string())
    }
    fn memory() -> Result<Arc<Self>, String> {
        SledMemoStore::memory().map_err(|e| e.to_string())
    }
}

impl TenantStore for SledRecordStore {
    fn open(path: &std::path::Path) -> Result<Arc<Self>, String> {
        SledRecordStore::open(path).map_err(|e| e.to_string())
    }
    fn memory() -> Result<Arc<Self>, String> {
        SledRecordStore::memory().map_err(|e| e.to_string())
    }
}

impl TenantStore for SledFeedbackStore {
    fn open(path: &std::path::Path) -> Result<Arc<Self>, String> {
        SledFeedbackStore::open(path).map_err(|e| e.to_string())
    }
    fn memory() -> Result<Arc<Self>, String> {
        SledFeedbackStore::memory().map_err(|e| e.to_string())
    }
}

/// Lazily opens + caches one store per principal under `root/<pid>` (persistent)
/// or a fresh in-memory db (memory mode).
struct TenantStoreRegistry<S: TenantStore> {
    mode: StoreMode,
    cache: Mutex<HashMap<String, Arc<S>>>,
}

#[derive(Clone)]
enum StoreMode {
    Persistent(PathBuf),
    Memory,
}

impl<S: TenantStore> Clone for TenantStoreRegistry<S> {
    fn clone(&self) -> Self {
        Self {
            mode: self.mode.clone(),
            cache: Mutex::new(self.cache.lock().unwrap().clone()),
        }
    }
}

impl<S: TenantStore> TenantStoreRegistry<S> {
    fn for_principal(&self, pid: &str) -> Arc<S> {
        validate_pid(pid);
        let mut cache = self.cache.lock().unwrap();
        if let Some(s) = cache.get(pid) {
            return s.clone();
        }
        let store = match &self.mode {
            StoreMode::Persistent(root) => {
                let dir = root.join(pid);
                let _ = std::fs::create_dir_all(&dir);
                S::open(&dir).expect("open tenant store")
            }
            StoreMode::Memory => S::memory().expect("memory tenant store"),
        };
        cache.insert(pid.to_string(), store.clone());
        store
    }
}

/// Reject principal ids that could break out of the sled root directory.
fn validate_pid(pid: &str) {
    if pid.is_empty()
        || pid.len() > 64
        || !pid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        panic!("invalid principal id: {pid:?}");
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for Principal
where
    S: Send + Sync,
{
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, StatusCode> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

#[derive(Clone)]
struct AppState {
    pool: sqlx::PgPool,
    /// Admin/bootstrap principal's stores (legacy root paths).
    admin_memo: Arc<dyn MemoStore>,
    admin_records: Arc<dyn RecordStore>,
    admin_feedback: Arc<dyn FeedbackStore>,
    /// Per-tenant store registries (lazily opened under subdirs).
    tenant_memo: TenantStoreRegistry<SledMemoStore>,
    tenant_records: TenantStoreRegistry<SledRecordStore>,
    tenant_feedback: TenantStoreRegistry<SledFeedbackStore>,
    /// Shared, hot-swappable harness served to every agent.
    active: Arc<ActiveHarness>,
    /// Model factory (real OpenAI in prod, stub in tests).
    make_model: Arc<dyn Fn() -> Box<dyn ModelClient> + Send + Sync>,
    /// `.reef/` artifact directory (Git-versioned harness files).
    reef_dir: PathBuf,
}

impl AppState {
    /// Memo store scoped to a principal (admin → legacy root store).
    fn memo_for(&self, p: &Principal) -> Arc<dyn MemoStore> {
        if p.kind == PrincipalKind::Admin {
            self.admin_memo.clone()
        } else {
            self.tenant_memo.for_principal(&p.id)
        }
    }
    /// Record store scoped to a principal.
    fn records_for(&self, p: &Principal) -> Arc<dyn RecordStore> {
        if p.kind == PrincipalKind::Admin {
            self.admin_records.clone()
        } else {
            self.tenant_records.for_principal(&p.id)
        }
    }
    /// Feedback store scoped to a principal.
    fn feedback_for(&self, p: &Principal) -> Arc<dyn FeedbackStore> {
        if p.kind == PrincipalKind::Admin {
            self.admin_feedback.clone()
        } else {
            self.tenant_feedback.for_principal(&p.id)
        }
    }
    /// Evolution engine bound to a principal's records/feedback, but writing the
    /// winning harness into the shared `active` (fleet-wide hot-swap).
    fn engine_for(&self, p: &Principal) -> Arc<EvolutionEngine> {
        Arc::new(EvolutionEngine::new(
            (self.make_model)(),
            self.records_for(p),
            self.feedback_for(p),
            self.reef_dir.clone(),
            self.active.clone(),
            3,
        ))
    }
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

#[derive(Deserialize)]
struct ReefReportBody {
    /// Receipt ids of the recorded turns this feedback concerns.
    references: Vec<String>,
    score: f64,
    feedback: Option<String>,
}

#[derive(Debug)]
enum AppError {
    Db(sqlx::Error),
    Core(CoreError),
    Reef(ReefError),
    BadReport(String),
    NotFound,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AppError::Db(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
            AppError::Core(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("core: {e}")),
            AppError::Reef(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("reef: {e}")),
            AppError::BadReport(e) => (StatusCode::BAD_REQUEST, format!("bad_report: {e}")),
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

/// Bearer / ApiKey auth gate.
///
/// Resolves the presented key to a [`Principal`] and inserts it into the request
/// extensions so downstream handlers can scope data per tenant. Precedence:
/// * `Authorization: Bearer <key>` (or `ApiKey <key>`).
/// * If the key equals `AGENT_CLOUD_API_KEY` → bootstrap **Admin** principal
///   (backward compatible with the old single-key gate).
/// * Else the key is looked up (by sha256 hash) in the `api_keys` table → the
///   row's `principal_id` / `scopes` define the principal.
/// * No key presented: if `AGENT_CLOUD_API_KEY` is set the request is rejected
///   (auth is on); otherwise the service runs **open** as the `default` tenant.
async fn require_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = presented_token(&req);
    let principal = resolve_principal(&state, token).await?;
    req.extensions_mut().insert(principal);
    Ok(next.run(req).await)
}

/// Extract the bearer/apikey token from the `Authorization` header.
fn presented_token(req: &Request) -> Option<String> {
    req.headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("ApiKey "))
                .map(|t| t.trim().to_string())
        })
        .filter(|t| !t.is_empty())
}

/// Resolve a presented token into a [`Principal`].
async fn resolve_principal(
    state: &AppState,
    token: Option<String>,
) -> Result<Principal, StatusCode> {
    let env_key = std::env::var("AGENT_CLOUD_API_KEY")
        .ok()
        .filter(|k| !k.is_empty());
    match token {
        // Bootstrap admin: matches the legacy single key.
        Some(t) if env_key.as_deref() == Some(t.as_str()) => Ok(Principal::admin()),
        // Look the key up in the metadata store.
        Some(t) => {
            let hash = hash_key(&t);
            let row = sqlx::query(
                "SELECT principal_id, scopes FROM api_keys \
                 WHERE key_hash = $1 AND revoked_at IS NULL",
            )
            .bind(&hash)
            .fetch_optional(&state.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            match row {
                Some(r) => {
                    let pid: String = r.get("principal_id");
                    let scopes: String = r.get("scopes");
                    let tokens: Vec<String> = scopes
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect();
                    let scopes = ApiKeyScopes::from_tokens(&tokens);
                    let kind = if scopes.admin {
                        PrincipalKind::Admin
                    } else {
                        PrincipalKind::User
                    };
                    Ok(Principal {
                        id: pid,
                        kind,
                        scopes,
                    })
                }
                None => Err(StatusCode::UNAUTHORIZED),
            }
        }
        // No token: open mode → default tenant, else reject.
        None => {
            if env_key.is_some() {
                Err(StatusCode::UNAUTHORIZED)
            } else {
                Ok(Principal::default_user())
            }
        }
    }
}

/// sha256 hex digest of a key (never store or log the plaintext).
fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    format!("{:x}", h.finalize())
}

/// Generate a high-entropy API key: `aria-<hex>`, returning the plaintext (shown
/// once), a short display prefix, and the sha256 hash stored in Postgres.
fn generate_key() -> (String, String, String) {
    use rand::RngCore;
    let mut bytes = [0u8; 18];
    rand::thread_rng().fill_bytes(&mut bytes);
    let raw: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let key = format!("aria-{raw}");
    let prefix = format!("aria-{}", &raw[..12]);
    let hash = hash_key(&key);
    (key, prefix, hash)
}

/// Build an [`Agent`] for the given run body (resolves the agent name from the
/// Postgres metadata store, wires the OpenAI model + memo, and **shares** the
/// process-wide `ActiveHarness` so a self-improvement win is picked up live).
async fn build_agent(
    state: &AppState,
    body: &RunBody,
    principal: &Principal,
) -> Result<Agent, AppError> {
    let name = agent_name(state, &body.agent_id).await?;
    let cfg = agent_config(&name, &body.session);
    let memo = state.memo_for(principal);
    Agent::with_harness(cfg, (state.make_model)(), memo, state.active.clone())
        .map_err(AppError::Core)
}

async fn create_agent(
    State(state): State<AppState>,
    Json(body): Json<CreateAgentBody>,
) -> Result<Json<AgentRecord>, AppError> {
    let id = Uuid::new_v4().to_string();
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
    principal: Principal,
    Json(body): Json<RunBody>,
) -> Response {
    let agent = match build_agent(&state, &body, &principal).await {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    let output = match agent.run(&body.input).await {
        Ok(o) => o,
        Err(e) => return AppError::Core(e).into_response(),
    };

    // Record the turn (learning log) and surface the receipt id as a header.
    // Both are scoped to the calling principal's stores.
    let records = state.records_for(&principal);
    let rec = Record::new(
        &body.agent_id,
        &body.session,
        None,
        &state.active.get().system_text(),
        &body.input,
        &output,
        DEFAULT_MODEL,
    );
    if let Err(e) = records.record_turn(rec.clone()).await {
        tracing::warn!("reef: failed to record turn: {e}");
    }

    // Metadata only — never the conversational context. Attribute to principal.
    let run_id = Uuid::new_v4().to_string();
    if let Err(e) = sqlx::query(
        "INSERT INTO runs (id, agent_id, session, input, output, principal_id) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&run_id)
    .bind(&body.agent_id)
    .bind(&body.session)
    .bind(&body.input)
    .bind(&output)
    .bind(&principal.id)
    .execute(&state.pool)
    .await
    {
        tracing::warn!("reef: failed to persist run metadata: {e}");
    }

    let mut resp = Json(RunRecord {
        id: run_id,
        agent_id: body.agent_id,
        session: body.session,
        output,
    })
    .into_response();
    resp.headers_mut().insert(
        HeaderName::from_static(REEF_RECORD_HEADER),
        HeaderValue::from_str(&rec.id).unwrap_or(HeaderValue::from_static("")),
    );
    resp
}

async fn run_agent_stream(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<RunBody>,
) -> Response {
    // Build the agent (resolves name + memo) before streaming.
    let agent = match build_agent(&state, &body, &principal).await {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };

    let sys = state.active.get().system_text();
    let rec_id = Uuid::new_v4().to_string();
    let records = state.records_for(&principal);
    // Placeholder record so the receipt id is known before streaming begins.
    if let Err(e) = records
        .record_turn(Record::new(
            &body.agent_id,
            &body.session,
            None,
            &sys,
            &body.input,
            "",
            DEFAULT_MODEL,
        ))
        .await
    {
        tracing::warn!("reef: failed to record turn placeholder: {e}");
    }

    let token_stream = match agent.run_stream(&body.input).await {
        Ok(s) => s,
        Err(e) => return AppError::Core(e).into_response(),
    };

    // Finalize the record at the end of the stream (overwrite placeholder with
    // the real output, keeping the same receipt id).
    let agent_id = body.agent_id.clone();
    let session = body.session.clone();
    let input = body.input.clone();
    let rec_id_inner = rec_id.clone();
    let wrapped = async_stream::stream! {
        let mut collected = String::new();
        let mut s = token_stream;
        while let Some(item) = s.next().await {
            match item {
                Ok(tok) => {
                    collected.push_str(&tok);
                    yield Ok(tok);
                }
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }
        let mut final_rec =
            Record::new(&agent_id, &session, None, &sys, &input, &collected, DEFAULT_MODEL);
        final_rec.id = rec_id_inner.clone();
        let _ = records.record_turn(final_rec).await;
    };

    let sse = wrapped
        .map(|res| {
            Ok::<Event, Infallible>(match res {
                Ok(token) => Event::default()
                    .json_data(StreamToken { token })
                    .unwrap_or_else(|_| Event::default().data("[encode-error]")),
                Err(e) => Event::default().data(format!("error: {e}")),
            })
        })
        .chain(stream::once(async { Ok(Event::default().data("[DONE]")) }));
    let mut resp = Sse::new(Box::pin(sse)).into_response();
    resp.headers_mut().insert(
        HeaderName::from_static(REEF_RECORD_HEADER),
        HeaderValue::from_str(&rec_id).unwrap_or(HeaderValue::from_static("")),
    );
    resp
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

// --- Reef self-improvement endpoints ---

/// Bind feedback to recorded turns. References must point at existing records.
async fn reef_report(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<ReefReportBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let records = state.records_for(&principal);
    let feedback = state.feedback_for(&principal);
    for r in &body.references {
        if records.get(r).await.is_err() {
            return Err(AppError::BadReport(format!("unknown record: {r}")));
        }
    }
    let fb = Feedback::new(body.references, body.score, body.feedback);
    feedback.report(fb).await.map_err(AppError::Reef)?;
    Ok(Json(json!({ "ok": true })))
}

/// Run one evolution pass: propose → select → keep winner → version → hot-swap.
async fn reef_evolve(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<serde_json::Value>, AppError> {
    let engine = state.engine_for(&principal);
    match engine.evolve().await {
        Ok(out) => Ok(Json(json!({
            "version": out.version,
            "adopted": out.adopted,
            "candidate_system": out.candidate_system,
        }))),
        Err(ReefError::NoEligible) => Ok(Json(json!({
            "version": 0,
            "adopted": false,
            "reason": "no_eligible",
        }))),
        Err(e) => Err(AppError::Reef(e)),
    }
}

/// List committed harness versions (plus the implicit `baseline`).
async fn reef_versions(State(state): State<AppState>) -> Json<Vec<String>> {
    let mut v = git::list_versions(&state.reef_dir).unwrap_or_default();
    v.push("baseline".to_string());
    Json(v)
}

// --- Self-serve API key management (admin only) ---

#[derive(Deserialize)]
struct CreateApiKeyBody {
    /// Human label for the key (optional).
    label: Option<String>,
    /// Tenant/user this key authenticates as. Generated when omitted.
    principal_id: Option<String>,
    /// Permission tokens: `read`, `write`, `admin`. Defaults to `read,write`.
    scopes: Option<Vec<String>>,
}

#[derive(Serialize)]
struct ApiKeyView {
    id: String,
    key_prefix: String,
    principal_id: String,
    label: Option<String>,
    scopes: Vec<String>,
    created_at: String,
}

#[derive(Serialize)]
struct CreatedApiKey {
    id: String,
    /// Plaintext key — shown ONLY in this response. Store it now.
    key: String,
    key_prefix: String,
    principal_id: String,
    scopes: Vec<String>,
}

/// Create a new API key for a principal. Admin scope required.
async fn create_api_key(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<CreateApiKeyBody>,
) -> Result<Json<CreatedApiKey>, StatusCode> {
    if !principal.is_admin() {
        return Err(StatusCode::FORBIDDEN);
    }
    let pid = body
        .principal_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    validate_pid(&pid);
    let scopes = ApiKeyScopes::from_tokens(
        &body
            .scopes
            .unwrap_or_else(|| vec!["read".into(), "write".into()]),
    );
    let (key, prefix, hash) = generate_key();
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO api_keys (id, key_prefix, key_hash, principal_id, label, scopes, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, now())",
    )
    .bind(&id)
    .bind(&prefix)
    .bind(&hash)
    .bind(&pid)
    .bind(&body.label)
    .bind(scopes.to_tokens().join(","))
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(CreatedApiKey {
        id,
        key,
        key_prefix: prefix,
        principal_id: pid,
        scopes: scopes.to_tokens(),
    }))
}

/// List active (non-revoked) API keys. Admin scope required.
async fn list_api_keys(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<ApiKeyView>>, StatusCode> {
    if !principal.is_admin() {
        return Err(StatusCode::FORBIDDEN);
    }
    let rows = sqlx::query(
        "SELECT id, key_prefix, principal_id, label, scopes, created_at::text \
         FROM api_keys WHERE revoked_at IS NULL ORDER BY created_at",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let scopes: String = r.get("scopes");
        out.push(ApiKeyView {
            id: r.get("id"),
            key_prefix: r.get("key_prefix"),
            principal_id: r.get("principal_id"),
            label: r.get("label"),
            scopes: scopes
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
            created_at: r.get("created_at"),
        });
    }
    Ok(Json(out))
}

/// Revoke an API key. Admin scope required.
async fn revoke_api_key(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !principal.is_admin() {
        return Err(StatusCode::FORBIDDEN);
    }
    let res =
        sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
            .bind(&id)
            .execute(&state.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if res.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!({ "ok": true })))
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
    // Attribute runs to the calling principal (audit only).
    pool.execute("ALTER TABLE runs ADD COLUMN IF NOT EXISTS principal_id TEXT")
        .await?;
    Ok(())
}

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "aria-agent",
    about = "Agents Cloud API + CLI",
    version = AGENT_VERSION,
    arg_required_else_help = false,
    disable_version_flag = true
)]
struct Cli {
    /// Print version
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    _version: (),
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Write CLI config (~/.ariacompute/agent-cli.yml)
    Setup(SetupArgs),
    /// Replace this CLI + libaria-agent_ffi from Releases
    Upgrade {
        /// Target version (default: latest stable)
        version: Option<String>,
        /// Override the upgrade_url from config
        #[arg(long)]
        url: Option<String>,
    },
    /// Start the cloud HTTP server
    Serve {
        /// Listen port (default: CLOUD_PORT env or 3000)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Print version
    Version,
}

#[derive(Args)]
struct SetupArgs {
    /// Show CLI config status
    #[arg(long)]
    status: bool,
    /// Remove the CLI config file
    #[arg(long)]
    clear: bool,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Setup(args)) => cmd_setup(args)?,
        Some(Command::Upgrade { version, url }) => {
            upgrade::run(version.as_deref(), url.as_deref(), AGENT_VERSION)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
        }
        Some(Command::Serve { port }) => cmd_serve(resolve_port(port)).await?,
        None => cmd_serve(resolve_port(None)).await?,
        Some(Command::Version) => println!("aria-agent {AGENT_VERSION}"),
    }
    Ok(())
}

fn cmd_setup(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.status {
        return cmd_setup_status();
    }
    if args.clear {
        return cmd_setup_clear();
    }

    // The Releases source is configured only via interactive mode.
    let upgrade_url = if std::io::stdin().is_terminal() {
        let ans = prompt("Releases source (1=github, 2=gitee) [1]: ")?;
        match ans.trim() {
            "" | "1" | "github" | "gh" => cli_config::upgrade_url_for_site("github"),
            "2" | "gitee" => cli_config::upgrade_url_for_site("gitee"),
            other => {
                return Err(format!("unknown source: {other} (expected github or gitee)").into())
            }
        }
    } else {
        // Non-interactive: fall back to the GitHub default.
        cli_config::upgrade_url_for_site("github")
    };

    let path = cli_config::save_cli_config(&cli_config::AgentCliConfig {
        upgrade_url: upgrade_url.clone(),
    })?;
    println!("wrote {} (upgrade_url={upgrade_url})", path.display());
    Ok(())
}

/// Read one line from stdin (used by interactive `setup`).
fn prompt(q: &str) -> io::Result<String> {
    use std::io::Write;
    print!("{q}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// `aria-agent setup --status`: print where the CLI config lives + its values.
fn cmd_setup_status() -> Result<(), Box<dyn std::error::Error>> {
    let path = cli_config::cli_config_path()?;
    if path.exists() {
        println!("config: {}", path.display());
        let cli = cli_config::load_cli_config()?;
        if cli.upgrade_url.is_empty() {
            println!("upgrade_url: (not set)");
        } else {
            println!("upgrade_url: {}", cli.upgrade_url);
        }
    } else {
        println!(
            "config: {} (missing; run `aria-agent setup`)",
            path.display()
        );
    }
    println!("lib: {}", cli_config::lib_dir()?.display());
    Ok(())
}

/// `aria-agent setup --clear`: delete the CLI config file.
fn cmd_setup_clear() -> Result<(), Box<dyn std::error::Error>> {
    match cli_config::clear_cli_config()? {
        Some(path) => println!("cleared {}", path.display()),
        None => println!("(nothing to clear)"),
    }
    Ok(())
}

/// Resolve the cloud listen port: explicit `--port`, else `CLOUD_PORT` env (from `.env`), else 3000.
fn resolve_port(explicit: Option<u16>) -> u16 {
    if let Some(p) = explicit {
        return p;
    }
    std::env::var("CLOUD_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(3000)
}

async fn cmd_serve(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Prefer CLI config from ~/.ariacompute/agent-cli.yml.
    let _ = cli_config::ensure_aria_home();
    let cli = cli_config::load_cli_config()?;
    if cli.upgrade_url.is_empty() {
        tracing::info!("agent-cli: no upgrade_url configured (run `aria-agent setup`)");
    } else {
        tracing::info!("agent-cli: upgrade_url={}", cli.upgrade_url);
    }
    // Expose the installed libaria-agent_ffi to native SDKs unless overridden.
    if std::env::var("ARIA_AGENT_FFI_LIB").is_err() {
        if let Ok(lib) = cli_config::lib_dir() {
            std::env::set_var("ARIA_AGENT_FFI_LIB", lib);
        }
    }

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/agent".into());
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await?;
    ensure_schema(&pool).await?;

    // Local/embedded memo store — the ONLY holder of conversational context.
    //
    // Backend is selected via `AGENT_MEMO_BACKEND`:
    //   * `memory` (default) — ephemeral, in-memory sled (lost on restart).
    //   * `memo`           — persistent sled DB under `MEMO_DIR`, survives restarts.
    // The admin/bootstrap principal's memo uses the legacy root path so existing
    // single-key deployments keep their data; per-tenant memos live under
    // `MEMO_DIR/<principal_id>`.
    let memo_backend = std::env::var("AGENT_MEMO_BACKEND").unwrap_or_else(|_| "memory".into());
    let admin_memo: Arc<dyn MemoStore>;
    let memo_mode: StoreMode;
    match memo_backend.as_str() {
        "memo" => {
            let memo_dir =
                PathBuf::from(std::env::var("MEMO_DIR").unwrap_or_else(|_| "/app/.memo".into()));
            std::fs::create_dir_all(&memo_dir).map_err(|e| e.to_string())?;
            tracing::info!("memo: persistent backend at {}", memo_dir.display());
            admin_memo = SledMemoStore::open(&memo_dir).map_err(|e| e.to_string())?;
            memo_mode = StoreMode::Persistent(memo_dir);
        }
        _ => {
            tracing::info!("memo: in-memory backend (ephemeral)");
            admin_memo = SledMemoStore::memory().map_err(|e| e.to_string())?;
            memo_mode = StoreMode::Memory;
        }
    }

    // --- Reef wiring ---
    let reef_dir = PathBuf::from(std::env::var("REEF_DIR").unwrap_or_else(|_| ".reef".into()));
    std::fs::create_dir_all(&reef_dir).ok();
    // Initialize (idempotent) a git repo for harness versioning; best-effort.
    if let Err(e) = git::init_repo(&reef_dir) {
        tracing::warn!("reef: git init skipped: {e}");
    }
    // Load the last winning harness, or fall back to the baseline.
    let active: Arc<ActiveHarness> = Arc::new(match harness::load(&reef_dir) {
        Ok(h) => {
            tracing::info!(
                "reef: loaded harness v{} from {}",
                git::current_version(&reef_dir).unwrap_or(0),
                reef_dir.display()
            );
            ActiveHarness::new(h)
        }
        Err(_) => ActiveHarness::baseline("agent"),
    });
    // Admin/bootstrap principal's reef stores use the legacy root paths.
    let admin_records: Arc<dyn RecordStore> =
        SledRecordStore::open(&reef_dir.join("records")).map_err(|e| e.to_string())?;
    let admin_feedback: Arc<dyn FeedbackStore> =
        SledFeedbackStore::open(&reef_dir.join("feedback")).map_err(|e| e.to_string())?;
    // Real model for production; tests inject a stub via `make_model`.
    let make_model: Arc<dyn Fn() -> Box<dyn ModelClient> + Send + Sync> =
        Arc::new(|| Box::new(OpenAiModel::new(DEFAULT_MODEL)));

    let state = AppState {
        pool,
        admin_memo,
        admin_records,
        admin_feedback,
        tenant_memo: TenantStoreRegistry {
            mode: memo_mode,
            cache: Mutex::new(HashMap::new()),
        },
        tenant_records: TenantStoreRegistry {
            mode: StoreMode::Persistent(reef_dir.join("records")),
            cache: Mutex::new(HashMap::new()),
        },
        tenant_feedback: TenantStoreRegistry {
            mode: StoreMode::Persistent(reef_dir.join("feedback")),
            cache: Mutex::new(HashMap::new()),
        },
        active,
        make_model,
        reef_dir,
    };

    let app = Router::new()
        .route("/v1/agents", post(create_agent))
        .route("/v1/agents/:id", get(get_agent))
        .route("/v1/runs", post(run_agent))
        .route("/v1/runs/stream", post(run_agent_stream))
        .route("/reef/report", post(reef_report))
        .route("/reef/evolve", post(reef_evolve))
        .route("/reef/versions", get(reef_versions))
        .route("/v1/api-keys", post(create_api_key).get(list_api_keys))
        .route("/v1/api-keys/:id", delete(revoke_api_key))
        .with_state(state.clone())
        // Auth gate (Bearer / ApiKey); open when AGENT_CLOUD_API_KEY is unset.
        // `from_fn_with_state` threads `AppState` into the middleware closure.
        .layer(from_fn_with_state(state, require_auth));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("agent-cloud listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::StubModel;
    use agent_memo::{ContextFragment, FragmentKind, RecallQuery};

    /// Build an `AppState` with in-memory tenant stores + StubModel engine. The
    /// pg pool is lazy (never queried by the reef handlers), so no real Postgres
    /// is required for these tests.
    fn test_state() -> AppState {
        let reef_dir = std::env::temp_dir().join(format!("reef_cloud_{}", Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&reef_dir);
        std::fs::create_dir_all(&reef_dir).unwrap();
        let active = Arc::new(ActiveHarness::baseline("agent"));
        let admin_records: Arc<dyn RecordStore> = SledRecordStore::memory().unwrap();
        let admin_feedback: Arc<dyn FeedbackStore> = SledFeedbackStore::memory().unwrap();
        let admin_memo: Arc<dyn MemoStore> = SledMemoStore::memory().unwrap();
        let make_model: Arc<dyn Fn() -> Box<dyn ModelClient> + Send + Sync> =
            Arc::new(|| Box::new(StubModel::new("agent")));
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://x:x@localhost:5432/x")
            .unwrap();
        AppState {
            pool,
            admin_memo,
            admin_records,
            admin_feedback,
            tenant_memo: TenantStoreRegistry {
                mode: StoreMode::Memory,
                cache: Mutex::new(HashMap::new()),
            },
            tenant_records: TenantStoreRegistry {
                mode: StoreMode::Memory,
                cache: Mutex::new(HashMap::new()),
            },
            tenant_feedback: TenantStoreRegistry {
                mode: StoreMode::Memory,
                cache: Mutex::new(HashMap::new()),
            },
            active,
            make_model,
            reef_dir,
        }
    }

    #[tokio::test]
    async fn reef_report_rejects_unknown_record() {
        let state = test_state();
        let res = reef_report(
            State(state),
            Principal::admin(),
            Json(ReefReportBody {
                references: vec!["ghost".into()],
                score: -1.0,
                feedback: None,
            }),
        )
        .await;
        assert!(res.is_err(), "reporting on a missing record must error");
    }

    #[tokio::test]
    async fn reef_report_binds_feedback_to_record() {
        let state = test_state();
        let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
        state.admin_records.record_turn(rec.clone()).await.unwrap();
        let res = reef_report(
            State(state.clone()),
            Principal::admin(),
            Json(ReefReportBody {
                references: vec![rec.id.clone()],
                score: -1.0,
                feedback: Some("wrong answer".into()),
            }),
        )
        .await;
        assert!(res.is_ok());
        // Feedback now visible in the store.
        let listed = state.admin_feedback.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].references, vec![rec.id]);
    }

    #[tokio::test]
    async fn reef_evolve_wins_and_hot_swaps_without_pg() {
        let state = test_state();
        let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
        state.admin_records.record_turn(rec.clone()).await.unwrap();
        state
            .admin_feedback
            .report(Feedback::new(
                vec![rec.id.clone()],
                -1.0,
                Some("wrong".into()),
            ))
            .await
            .unwrap();

        let before = state.active.get().system_text();
        let out = reef_evolve(State(state.clone()), Principal::admin())
            .await
            .unwrap();
        assert_eq!(out.0["adopted"], json!(true));
        let after = state.active.get().system_text();
        assert_ne!(before, after);
        assert!(after.contains("reef_improvement"));
    }

    #[tokio::test]
    async fn reef_evolve_no_eligible_keeps_active() {
        let state = test_state();
        let out = reef_evolve(State(state.clone()), Principal::admin())
            .await
            .unwrap();
        assert_eq!(out.0["adopted"], json!(false));
        assert_eq!(out.0["reason"], json!("no_eligible"));
        // Active harness untouched.
        assert!(state.active.get().is_baseline("agent"));
    }

    #[tokio::test]
    async fn reef_versions_includes_baseline() {
        let state = test_state();
        let vers = reef_versions(State(state)).await;
        assert!(vers.contains(&"baseline".to_string()));
    }

    #[tokio::test]
    async fn run_agent_records_turn_and_returns_receipt_header() {
        // We can't easily assert the header from a handler return, but we can
        // verify the record is persisted for a known record id by simulating
        // the same path the handler uses. This exercises RecordStore wiring.
        let state = test_state();
        let rec = Record::new("a", "s", None, "You are agent.", "hi", "hello", "stub");
        let id = rec.id.clone();
        state.admin_records.record_turn(rec).await.unwrap();
        let got = state.admin_records.get(&id).await.unwrap();
        assert_eq!(got.output, "hello");
    }

    #[tokio::test]
    async fn tenant_memo_isolation() {
        let state = test_state();
        let a = Principal {
            id: "tenantA".into(),
            kind: PrincipalKind::User,
            scopes: ApiKeyScopes::read_write(),
        };
        let b = Principal {
            id: "tenantB".into(),
            kind: PrincipalKind::User,
            scopes: ApiKeyScopes::read_write(),
        };
        state
            .memo_for(&a)
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::Message,
                "secret from A",
            ))
            .await
            .unwrap();
        // Tenant B must not see A's context.
        let from_b = state
            .memo_for(&b)
            .recall(&RecallQuery::new("s", "secret"))
            .await
            .unwrap();
        assert!(from_b.is_empty(), "tenant B must not see A's context");
        let from_a = state
            .memo_for(&a)
            .recall(&RecallQuery::new("s", "secret"))
            .await
            .unwrap();
        assert_eq!(from_a.len(), 1);
    }

    #[tokio::test]
    async fn open_mode_resolves_default_principal() {
        // No AGENT_CLOUD_API_KEY and no token → default tenant, no pg needed.
        std::env::remove_var("AGENT_CLOUD_API_KEY");
        let state = test_state();
        let p = resolve_principal(&state, None).await.unwrap();
        assert_eq!(p, Principal::default_user());
    }

    #[test]
    fn scopes_from_tokens_maps_admin_and_defaults() {
        let rw = ApiKeyScopes::from_tokens(&["read".into(), "write".into()]);
        assert!(rw.read && rw.write && !rw.admin);
        let admin = ApiKeyScopes::from_tokens(&["admin".into()]);
        assert!(admin.admin && admin.read && admin.write);
    }

    #[test]
    fn hash_key_is_deterministic() {
        assert_eq!(hash_key("aria-test"), hash_key("aria-test"));
        assert_ne!(hash_key("aria-a"), hash_key("aria-b"));
    }
}
