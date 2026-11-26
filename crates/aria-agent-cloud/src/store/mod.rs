//! Data-access layer for the cloud API.
//!
//! Handlers only orchestrate: validate → call a store → map the result to the
//! OpenAI wire shape. Two implementations exist:
//!
//! * [`pg`] — Postgres (metadata + multi-tenant scoping), the production path.
//!   Excluded from the coverage gate (needs a live database).
//! * [`memory`] — in-process implementation with identical semantics, used by
//!   the unit/integration tests so the whole API is exercisable without a DB.

pub mod governance;
pub mod memory;
pub mod pg;
pub mod pg_orchestration;
pub mod pg_rsi;

pub use governance::PgGovernanceStore;
pub use memory::MemoryStore;
pub use pg::{ensure_schema, PgStore};
pub use pg_rsi::ensure_rsi_schema;

use agent_core::session::insight::Finding;
use async_trait::async_trait;
use std::sync::Arc;

use crate::auth::PrincipalKind;
use serde_json::Value;

use crate::auth::{ApiKeyScopes, Principal};
use crate::types::{Agent, CreateAgentRequest, UpdateAgentRequest};
use crate::AppError;

/// One row of `agent_sessions` as the API layer needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub principal_id: String,
    pub agent_id: String,
    pub instructions: Option<String>,
    pub status: String,
    /// Epoch milliseconds (the wire unit).
    pub created_at: i64,
}

/// One row of `session_turns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRow {
    pub id: String,
    pub principal_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub status: String,
    pub output: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

/// One row of `session_items`.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemRow {
    pub id: String,
    pub principal_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub kind: String,
    pub role: Option<String>,
    pub status: String,
    pub content: Value,
    pub seq: i32,
    pub created_at: i64,
}

/// A key row as returned by `GET /v1/api-keys`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRow {
    pub id: String,
    pub key_prefix: String,
    pub principal_id: String,
    pub label: Option<String>,
    pub scopes: Vec<String>,
    pub created_at: String,
}

/// A key row to insert (the plaintext is never persisted, only `key_hash`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApiKeyRow {
    pub id: String,
    pub key_prefix: String,
    pub key_hash: String,
    pub principal_id: String,
    pub label: Option<String>,
    pub scopes: Vec<String>,
}

/// A successfully resolved API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    pub principal_id: String,
    pub scopes: ApiKeyScopes,
}

/// Reusable agents, scoped by `principal_id` (admins see every tenant).
#[async_trait]
pub trait AgentStore: Send + Sync {
    async fn insert_agent(&self, principal: &Principal, agent: &Agent) -> Result<(), AppError>;
    async fn list_agents(&self, principal: &Principal, limit: i64) -> Result<Vec<Agent>, AppError>;
    /// Look up by id, then by human-friendly name (playground / quick start).
    async fn get_agent(&self, principal: &Principal, id_or_name: &str) -> Result<Agent, AppError>;
    async fn save_agent(&self, agent: &Agent) -> Result<(), AppError>;
    /// Delete a visible agent and return its id.
    async fn delete_agent(&self, principal: &Principal, id: &str) -> Result<String, AppError>;
}

/// Sessions, their turns and their items.
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn insert_session(
        &self,
        principal: &Principal,
        session: &SessionRow,
    ) -> Result<(), AppError>;
    async fn list_sessions(
        &self,
        principal: &Principal,
        limit: i64,
    ) -> Result<Vec<SessionRow>, AppError>;
    async fn get_session(&self, principal: &Principal, id: &str) -> Result<SessionRow, AppError>;
    async fn set_instructions(&self, id: &str, instructions: Option<&str>) -> Result<(), AppError>;
    async fn delete_session(&self, id: &str) -> Result<(), AppError>;

    async fn insert_turn(&self, turn: &TurnRow) -> Result<(), AppError>;
    async fn finish_turn(
        &self,
        id: &str,
        status: &str,
        output: Option<&str>,
    ) -> Result<(), AppError>;
    async fn list_turns(
        &self,
        principal: &Principal,
        session: &str,
    ) -> Result<Vec<TurnRow>, AppError>;
    async fn get_turn(
        &self,
        principal: &Principal,
        session: &str,
        turn: &str,
    ) -> Result<Option<TurnRow>, AppError>;

    async fn insert_item(&self, item: &ItemRow) -> Result<(), AppError>;
    async fn list_items(
        &self,
        principal: &Principal,
        session: &str,
    ) -> Result<Vec<ItemRow>, AppError>;
}

/// One row of `session_titles`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleRow {
    pub session_id: String,
    pub title: String,
    /// `first_prompt` (deterministic fallback) or `provider` (async override).
    pub source: String,
    /// Epoch milliseconds of the last write.
    pub updated_at: i64,
}

/// Session titles, scoped by `principal_id`.
///
/// Written by the runtime's title projection (`SessionEvent::TitleSet` → row)
/// and read by `GET /v1/agents/sessions/{id}/title`. The raw event stream stays
/// the source of truth: this table is a projection (ADR-0015).
#[async_trait]
pub trait TitleStore: Send + Sync {
    async fn put_title(&self, principal: &Principal, row: &TitleRow) -> Result<(), AppError>;
    async fn get_title(
        &self,
        principal: &Principal,
        session: &str,
    ) -> Result<Option<TitleRow>, AppError>;
}

/// Tenant visibility rule shared by every store: an admin sees everything, any
/// other principal sees only its own rows. One definition, so a new store cannot
/// quietly invent a laxer one.
pub(crate) fn can_see(principal: &Principal, owner: &str) -> bool {
    principal.kind == PrincipalKind::Admin || principal.id == owner
}

/// One persisted RSI report.
///
/// The *ledger* stays where it already lives (the session event stream); what is
/// stored here is the diagnosis — score, truncation flag and the evidence chain
/// — because that is the artifact a reviewer has to be able to find later.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub id: String,
    pub principal_id: String,
    pub session: String,
    pub score: i32,
    pub truncated: bool,
    pub findings: Vec<Finding>,
    /// Epoch milliseconds.
    pub created_at_ms: i64,
}

/// RSI diagnoses, scoped by `principal_id`.
#[async_trait]
pub trait RsiStore: Send + Sync {
    async fn insert_report(&self, row: &ReportRow) -> Result<(), AppError>;
    /// Newest first. Admins see every tenant, users only their own.
    async fn list_reports(
        &self,
        principal: &Principal,
        limit: i64,
    ) -> Result<Vec<ReportRow>, AppError>;
    async fn get_report(&self, principal: &Principal, id: &str) -> Result<ReportRow, AppError>;
}

/// Adapts [`RsiStore`] to the sweep's best-effort report sink.
///
/// The sweep only knows `SeamResult`; the API only knows `AppError`. This is the
/// one adapter between them, so a storage failure is reported as an `io` seam
/// error and the sweep logs it without aborting a diagnosis it already produced.
pub struct StoreReportSink {
    store: Arc<dyn RsiStore>,
}

impl StoreReportSink {
    pub fn new(store: Arc<dyn RsiStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl aria_agent_rsi::ReportSink for StoreReportSink {
    async fn record(
        &self,
        scope: &agent_core::seams::RsiScope,
        session: &str,
        report: &agent_core::session::insight::EfficiencyReport,
    ) -> Result<(), agent_core::seams::SeamError> {
        let row = ReportRow {
            id: uuid::Uuid::new_v4().to_string(),
            principal_id: scope.principal.clone(),
            session: session.to_string(),
            score: report.score,
            truncated: report.truncated,
            findings: report.findings.clone(),
            created_at_ms: agent_core::context::now_ms(),
        };
        self.store
            .insert_report(&row)
            .await
            .map_err(|e| agent_core::seams::SeamError::Io(format!("report store: {e:?}")))
    }
}

/// Self-serve API keys (admin-managed) plus the resolution path used by auth.
#[async_trait]
pub trait ApiKeyStore: Send + Sync {
    async fn insert_key(&self, row: &NewApiKeyRow) -> Result<(), AppError>;
    async fn list_keys(&self) -> Result<Vec<ApiKeyRow>, AppError>;
    /// Revoke a live key. Returns its hash when a row was updated, `None` when
    /// the id is unknown or already revoked (the handler maps that to 404).
    async fn revoke_key(&self, id: &str) -> Result<Option<String>, AppError>;
    /// Resolve a live key by its sha256 hash.
    async fn resolve_key(&self, key_hash: &str) -> Result<Option<ResolvedKey>, AppError>;
}

/// Build the wire `Agent` from a create request.
///
/// Pure so both stores (and the tests) share one interpretation of the
/// optional-field defaults.
pub fn agent_from_request(
    id: String,
    req: CreateAgentRequest,
    created_at: i64,
) -> Result<Agent, AppError> {
    let reasoning = req.reasoning.clone().unwrap_or_default();
    let multi_agent = req.multi_agent.clone().unwrap_or_default();
    let service_tier = req.service_tier.clone().unwrap_or_else(|| "auto".into());
    // `Vec<serde_json::Value>` is always serializable; validate cheaply here.
    let _ = serde_json::to_string(&req.tools.clone().unwrap_or_default())
        .expect("tool definitions are serializable");
    Ok(Agent {
        id,
        object: crate::types::AGENT_OBJECT.into(),
        name: req.name,
        model: req.model,
        instructions: req.instructions,
        reasoning,
        multi_agent,
        service_tier,
        text: req.text,
        tools: req.tools.unwrap_or_default(),
        created_at,
    })
}

/// Apply a partial update to an agent (only the provided fields change).
pub fn apply_agent_update(current: &mut Agent, req: UpdateAgentRequest) -> Result<(), AppError> {
    if let Some(name) = req.name {
        current.name = Some(name);
    }
    if let Some(model) = req.model {
        current.model = Some(model);
    }
    if let Some(instructions) = req.instructions {
        current.instructions = Some(instructions);
    }
    if let Some(r) = req.reasoning {
        current.reasoning = r;
    }
    if let Some(m) = req.multi_agent {
        current.multi_agent = m;
    }
    if let Some(t) = req.service_tier {
        current.service_tier = t;
    }
    if req.text.is_some() {
        current.text = req.text;
    }
    if let Some(tools) = req.tools {
        current.tools = tools;
    }
    Ok(())
}

/// The comma-joined scope string Postgres stores.
pub fn scopes_to_string(scopes: ApiKeyScopes) -> String {
    scopes.to_tokens().join(",")
}

/// Split the stored scope string back into tokens.
pub fn scopes_from_string(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}
