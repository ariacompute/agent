//! `aria-agent-cloud` — the Agents Cloud API as a library plus a thin binary.
//!
//! The binary (`src/main.rs`) only parses the CLI, boots Postgres and serves;
//! every piece of business logic lives here so it can be exercised by tests
//! without a database, a model provider or a listening socket.
//!
//! Modules:
//!
//! * [`api`] — OpenAI beta Agents handlers (agents / sessions / keys / stubs),
//! * [`app`] — shared state, agent construction, router, health probe,
//! * [`auth`] — principal resolution, API-key scopes and cache,
//! * [`store`] — data-access traits with Postgres + in-memory implementations,
//! * [`context`] — Postgres + pgvector [`agent_core::context::ContextStore`],
//! * [`event_envelope`] — the frozen `agent.*` SSE frame contract,
//! * [`types`] — wire types (OpenAI shapes),
//! * [`upgrade`] / [`cli_config`] — self-managed upgrade from Releases.

pub mod api;
#[cfg(test)]
pub(crate) mod api_tests;
pub mod app;
pub mod auth;
pub mod cli_config;
pub mod context;
/// Sandboxed dsh runtime: image/volume/hardening policy and boot preflight
/// (ADR-0027).
pub mod dsh_sandbox;
pub mod event_envelope;
pub mod governance;
pub mod internal_egress;
#[cfg(test)]
pub(crate) mod pg_testkit;
pub mod rsi_rerun;
#[cfg(test)]
mod rsi_tests;
pub mod store;
pub mod types;
pub mod upgrade;
pub mod util;

pub use app::{
    agent_config, agent_tools, build_agent, build_router, build_sse_stream, AppState, MakeContext,
    MakeModel, MakeSandbox, RsiState, DEFAULT_MODEL, HEALTH_PATH,
};
pub use auth::{
    generate_key, hash_key, presented_token, require_auth, resolve_principal, ApiKeyScopes,
    CachedKey, KeyCache, Principal, PrincipalKind,
};
pub use store::{
    agent_from_request, apply_agent_update, AgentStore, ApiKeyRow, ApiKeyStore, ItemRow,
    MemoryStore, NewApiKeyRow, PgStore, ResolvedKey, SessionRow, SessionStore, TurnRow,
};

use agent_core::CoreError;

/// Errors surfaced by the API layer. `IntoResponse` (the OpenAI-shaped body)
/// lives in [`api`].
#[derive(Debug)]
pub enum AppError {
    Db(sqlx::Error),
    Core(CoreError),
    BadRequest(String),
    Forbidden(String),
    NotFound,
    /// The caller's revision is stale (optimistic concurrency).
    Conflict(String),
    /// The request is well formed but cannot be applied as-is — for RSI, a
    /// cross-tenant playbook promotion without an adaptation record (ADR-0026).
    Unprocessable(String),
    /// The capability exists in the API but is not configured in this
    /// deployment (RSI disabled, no playbook provider).
    Unavailable(String),
}

/// Result alias used by the API layer.
pub type AppResult<T> = Result<T, AppError>;

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Db(e)
    }
}

impl From<agent_sandbox::SandboxError> for AppError {
    fn from(e: agent_sandbox::SandboxError) -> Self {
        AppError::Core(CoreError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn database_errors_keep_their_source() {
        let err: AppError = sqlx::Error::RowNotFound.into();
        assert!(matches!(err, AppError::Db(_)));
    }
}
