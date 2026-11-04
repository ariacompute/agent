//! Application wiring: shared state, agent construction and the HTTP router.
//!
//! Everything here is library code so it can be driven by tests through
//! `tower::ServiceExt::oneshot` — no listening socket, no database, no model
//! provider required.

use std::convert::Infallible;
use std::sync::Arc;

use agent_browser::{BrowserCatalog, BrowserSandbox, BrowserToolHandler};
use agent_core::context::ContextStore;
use agent_core::{Agent, AgentConfig, CoreError, ModelClient, Tool};
use agent_sandbox::{Sandbox, SandboxProvider};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::sse::Event;
use axum::response::{sse::Sse, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Value};

use crate::api;
use crate::auth::{require_auth, KeyCache, Principal};
use crate::store::{AgentStore, ApiKeyStore, SessionStore};
use crate::AppError;

/// Model used when neither the request nor deployment overrides it.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// Path of the unauthenticated liveness probe (see [`healthz`]).
pub const HEALTH_PATH: &str = "/healthz";

/// Model factory (real OpenAI in prod, stub in tests).
pub type MakeModel = Arc<dyn Fn() -> Box<dyn ModelClient> + Send + Sync>;
/// Sandbox factory (codex in prod, a cheap local implementation in tests).
pub type MakeSandbox = Arc<dyn Fn() -> Arc<dyn Sandbox> + Send + Sync>;
/// Context-store factory: one tenant-scoped store per principal.
pub type MakeContext = Arc<dyn Fn(&Principal) -> Arc<dyn ContextStore> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub agents: Arc<dyn AgentStore>,
    pub sessions: Arc<dyn SessionStore>,
    pub keys: Arc<dyn ApiKeyStore>,
    /// In-memory cache for API-key resolution (with revocation TTL).
    pub key_cache: KeyCache,
    pub make_model: MakeModel,
    pub make_sandbox: MakeSandbox,
    pub make_context: MakeContext,
}

impl AppState {
    /// Context store scoped to a principal.
    ///
    /// Conversational / long-term context lives in Postgres
    /// (`context_fragments`), sharded by `principal_id` — every query in
    /// `context::pg` filters on it.
    pub fn context_for(&self, p: &Principal) -> Arc<dyn ContextStore> {
        (self.make_context)(p)
    }
}

/// The frozen agentic toolset handed to every cloud run.
///
/// A `shell` exec tool whose `command` is a `[program, ...args]` array —
/// exactly the shape `Agent::run_event_stream` feeds to the sandbox. Execution
/// is delegated to the codex sandbox backend (ADR-0005), which is selected via
/// [`agent_config`]. The browser tool family is appended (no-op unless the run
/// selects `sandbox_provider: "browser"` and the runtime injects the
/// `BrowserToolHandler`).
pub fn agent_tools() -> Vec<Tool> {
    let mut tools = vec![Tool {
        name: "shell".into(),
        description:
            "Execute a shell command inside the agent sandbox and return its combined output."
                .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Program followed by its arguments, e.g. [\"ls\", \"-la\"]."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    }];
    tools.extend(agent_browser::browser_tools());
    tools
}

/// Build an [`Agent`] for the given run body: resolves the agent name from the
/// metadata store, wires the model, the principal-scoped context store and the
/// sandbox selected by [`agent_config`].
pub async fn build_agent(
    state: &AppState,
    name: &str,
    instructions: &str,
    session: &str,
    principal: &Principal,
) -> Result<Agent, AppError> {
    build_agent_with_config(state, agent_config(name, instructions, session), principal).await
}

/// Same as [`build_agent`] but with an explicit [`AgentConfig`].
///
/// Kept separate so every `sandbox_provider` branch (`codex`, `browser`,
/// docker-family, unknown) is reachable from tests without a live container:
/// constructing a provider is side-effect free, only `exec` touches a daemon.
pub async fn build_agent_with_config(
    state: &AppState,
    cfg: AgentConfig,
    principal: &Principal,
) -> Result<Agent, AppError> {
    let context = state.context_for(principal);
    tracing::debug!(
        principal = %principal.id,
        session = %cfg.session,
        "context: store resolved for principal"
    );
    // `codex` is provided by this runtime (publish = false); the published
    // `agent-sandbox` returns `NotConfigured` for it, so the factory injects
    // `CodexSandbox`. `browser` is likewise resolved here: a `BrowserSandbox`
    // (browser container manager + engine) is built and wrapped as a
    // `BrowserToolHandler`, while the agent's regular shell sandbox stays the
    // default Docker backend.
    let provider = SandboxProvider::parse(&cfg.sandbox_provider).ok_or_else(|| {
        AppError::Core(CoreError::Config(format!(
            "unknown sandbox: {}",
            cfg.sandbox_provider
        )))
    })?;
    let (sandbox, handlers): (Arc<dyn Sandbox>, Vec<Arc<dyn agent_core::ToolHandler>>) =
        match provider {
            SandboxProvider::Codex => ((state.make_sandbox)(), Vec::new()),
            SandboxProvider::Browser => {
                let browser = Arc::new(BrowserSandbox::with_catalog(
                    Box::new(agent_sandbox::DockerSandbox::new()),
                    BrowserCatalog::builtin(),
                    BrowserCatalog::default_kind(),
                ));
                let handler: Arc<dyn agent_core::ToolHandler> =
                    Arc::new(BrowserToolHandler::new(browser.clone()));
                // Shell fallback keeps using the default Docker sandbox.
                (Arc::new(agent_sandbox::DockerSandbox::new()), vec![handler])
            }
            other => (Arc::from(agent_sandbox::from_provider(other)?), Vec::new()),
        };
    Agent::with_handlers(cfg, (state.make_model)(), context, sandbox, handlers)
        .map_err(AppError::Core)
}

/// Wrap a translated frame stream into an SSE response.
///
/// Every streaming endpoint shares this single wire path: the frame stream is
/// produced by [`crate::event_envelope::EventEnvelope`], and this helper only
/// performs SSE encoding. No `[DONE]` sentinel is appended — the terminal
/// `agent.turn.completed` / `agent.turn.failed` frame is the end-of-stream
/// marker.
pub fn build_sse_stream(frames: impl futures::Stream<Item = Value> + Send + 'static) -> Response {
    // `frame` is already a `serde_json::Value`, so encoding it cannot fail:
    // `Event::data` writes the same `data: <json>` line `json_data` would.
    let sse = frames.map(|frame| Ok::<Event, Infallible>(Event::default().data(frame.to_string())));
    Sse::new(Box::pin(sse)).into_response()
}

pub fn agent_config(name: &str, instructions: &str, session: &str) -> AgentConfig {
    AgentConfig {
        session: session.to_string(),
        agent_name: name.to_string(),
        instructions: instructions.to_string(),
        // Tool execution is delegated to the codex sandbox backend (ADR-0005):
        // `SandboxProvider::Codex` is resolved by the runtime to
        // `codex_sandbox::CodexSandbox`, which runs commands through codex's
        // real `SandboxManager`. Docker/Kata/Cube remain available via
        // `agent_sandbox::from_provider`.
        sandbox_provider: "codex".into(),
        model: DEFAULT_MODEL.into(),
        // The cloud keeps the memory graph off by default (its context store
        // supports it; enable per-deployment if desired). The SDK enables it.
        memory_graph: false,
        memory_compact_threshold: 0,
    }
}

/// The HTTP surface: the OpenAI beta Agents resource plus self-serve API keys.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // --- beta Agents resource (OpenAI-compatible) ---
        .route(
            "/v1/agents",
            post(api::agents::create_agent).get(api::agents::list_agents),
        )
        .route(
            "/v1/agents/:agent_id",
            get(api::agents::get_agent)
                .post(api::agents::update_agent)
                .delete(api::agents::delete_agent),
        )
        .route(
            "/v1/agents/sessions",
            post(api::sessions::create_session).get(api::sessions::list_sessions),
        )
        .route(
            "/v1/agents/sessions/:session_id",
            get(api::sessions::get_session)
                .post(api::sessions::update_session)
                .delete(api::sessions::delete_session),
        )
        .route(
            "/v1/agents/sessions/:session_id/events",
            post(api::sessions::create_session_events),
        )
        .route(
            "/v1/agents/sessions/:session_id/events/stream",
            post(api::sessions::stream_session_events),
        )
        .route(
            "/v1/agents/sessions/:session_id/items",
            get(api::sessions::list_session_items),
        )
        .route(
            "/v1/agents/sessions/:session_id/turns",
            get(api::sessions::list_session_turns),
        )
        .route(
            "/v1/agents/sessions/:session_id/turns/:turn_id",
            get(api::sessions::get_session_turn),
        )
        .route(
            "/v1/agents/sessions/:session_id/memory",
            post(api::sessions::put_session_memory).get(api::sessions::search_session_memory),
        )
        .route(
            "/v1/agents/sessions/:session_id/memory/:key",
            get(api::sessions::get_session_memory),
        )
        .route(
            "/v1/agents/sessions/:session_id/memory/relations",
            post(api::sessions::create_session_memory_relation)
                .get(api::sessions::list_session_memory_relations)
                .delete(api::sessions::delete_session_memory_relations),
        )
        .route(
            "/v1/agents/sessions/:session_id/memory/relations/graph",
            get(api::sessions::graph_session_memory_relations),
        )
        .route(
            "/v1/agents/sessions/:session_id/subagents",
            get(api::sessions::list_subagents),
        )
        .route(
            "/v1/agents/sessions/:session_id/subagents/:subagent_id",
            get(api::sessions::get_subagent),
        )
        .route(
            "/v1/agents/sessions/:session_id/subagents/:subagent_id/items",
            get(api::sessions::list_subagent_items),
        )
        .route(
            "/v1/agents/sessions/:session_id/subagents/:subagent_id/turns",
            get(api::sessions::list_subagent_turns),
        )
        .route(
            "/v1/agents/sessions/:session_id/subagents/:subagent_id/turns/:turn_id",
            get(api::sessions::get_subagent_turn),
        )
        // --- recognised but unimplemented (501) ---
        .route(
            "/v1/vaults",
            get(api::stubs::vaults).post(api::stubs::vaults),
        )
        .route("/v1/vaults/:vault_id", get(api::stubs::vaults))
        .route(
            "/v1/vaults/:vault_id/credentials",
            get(api::stubs::vault_credentials),
        )
        .route(
            "/v1/agents/environments",
            get(api::stubs::environments).post(api::stubs::environments),
        )
        .route(
            "/v1/agents/environments/:environment_id",
            get(api::stubs::environments),
        )
        .route(
            "/v1/agents/environments/:environment_id/files",
            get(api::stubs::environment_files).post(api::stubs::environment_files),
        )
        .route(
            "/v1/agents/environments/templates",
            get(api::stubs::environment_templates),
        )
        .route(
            "/v1/agents/sessions/:session_id/artifacts",
            get(api::stubs::session_artifacts),
        )
        // --- self-serve API keys (admin only) ---
        .route(
            "/v1/api-keys",
            post(api::keys::create_api_key).get(api::keys::list_api_keys),
        )
        .route("/v1/api-keys/:id", delete(api::keys::revoke_api_key))
        .with_state(state.clone())
        // Auth gate (Bearer / ApiKey); open when AGENT_CLOUD_API_KEY is unset.
        // `from_fn_with_state` threads `AppState` into the middleware closure.
        .layer(from_fn_with_state(state, require_auth))
        // Liveness probe for container orchestration. Registered *after* the
        // auth layer on purpose: `layer` only wraps the routes added before it,
        // so `/healthz` answers 200 without credentials (see `healthz`).
        .route(HEALTH_PATH, get(healthz))
}

/// Unauthenticated liveness probe used by Docker/Compose health checks and
/// upstream load balancers.
///
/// Returns `200 ok` as soon as the process accepts HTTP. It deliberately does
/// **not** touch the database: a transient DB blip must not flap the container
/// (Postgres has its own healthcheck and `cloud` already waits for it via
/// `depends_on`).
pub async fn healthz() -> Response {
    (StatusCode::OK, "ok\n").into_response()
}

/// `Json` helper used by the API-key handlers (kept here so the response type
/// stays identical across handlers).
pub type JsonResponse<T> = Json<T>;
