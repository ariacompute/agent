//! Application wiring: shared state, agent construction and the HTTP router.
//!
//! Everything here is library code so it can be driven by tests through
//! `tower::ServiceExt::oneshot` — no listening socket, no database, no model
//! provider required.

use std::convert::Infallible;
use std::sync::Arc;

use agent_browser::{BrowserCatalog, BrowserSandbox, BrowserToolHandler};
use agent_core::context::ContextStore;
use agent_core::seams::{SeamError, SeamResult, SubagentRequest, Subagents};
use agent_core::session::{
    SessionError, SessionEvent, SessionPersistence, SessionRecord, SessionResult,
    SOURCE_FIRST_PROMPT,
};
use agent_core::{Agent, AgentConfig, CoreError, ModelClient, Tool};
use agent_sandbox::{Sandbox, SandboxProvider};
use aria_agent_orchestrator::{InProcessSubagents, SubagentFactory};
use async_trait::async_trait;
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
use crate::store::{AgentStore, ApiKeyStore, SessionStore, TitleRow, TitleStore};
use crate::AppError;

/// Model used when neither the request nor deployment overrides it.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// Path of the unauthenticated liveness probe (see [`healthz`]).
pub const HEALTH_PATH: &str = "/healthz";

/// Model factory (real OpenAI in prod, stub in tests).
pub type MakeModel = Arc<dyn Fn() -> Box<dyn ModelClient> + Send + Sync>;
/// Sandbox factory for the default (`docker`) provider: the real Docker Engine
/// backend in production, a cheap in-process stub in tests.
pub type MakeSandbox = Arc<dyn Fn() -> Arc<dyn Sandbox> + Send + Sync>;
/// Context-store factory: one tenant-scoped store per principal.
pub type MakeContext = Arc<dyn Fn(&Principal) -> Arc<dyn ContextStore> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub agents: Arc<dyn AgentStore>,
    pub sessions: Arc<dyn SessionStore>,
    pub keys: Arc<dyn ApiKeyStore>,
    /// Agent teams (`principal_id`-scoped in every provider).
    pub teams: Arc<dyn aria_agent_orchestrator::teams::TeamStore>,
    /// Scheduled jobs (`principal_id`-scoped in every provider).
    pub schedule: Arc<dyn agent_core::seams::ScheduleSeam>,
    /// Session titles: the `SessionEvent::TitleSet` projection.
    pub titles: Arc<dyn TitleStore>,
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
/// is delegated to the pluggable `sandbox` seam (selected via [`agent_config`]).
/// The browser tool family is appended (no-op unless the run selects
/// `sandbox_provider: "browser"` and the runtime injects the
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
/// Kept separate so every `sandbox_provider` branch (`browser`, docker-family,
/// unknown) is reachable from tests without a live container: constructing a
/// provider is side-effect free, only `exec` touches a daemon.
pub async fn build_agent_with_config(
    state: &AppState,
    cfg: AgentConfig,
    principal: &Principal,
) -> Result<Agent, AppError> {
    build_agent_inner(state, cfg, principal, true).await
}

/// The real constructor: `allow_subagents = false` builds a **child** agent.
///
/// Children never spawn children, so delegation depth is exactly one and the
/// fan-out stays bounded (`InProcessSubagents` also caps concurrency at
/// `DEFAULT_MAX_CONCURRENT`). Everything else — model, sandbox, context,
/// permission preset — is inherited from the parent config, and the child's own
/// session id (namespaced `<parent>::sub::<id>`) keeps its events separate
/// (ADR-0017).
pub(crate) async fn build_agent_inner(
    state: &AppState,
    cfg: AgentConfig,
    principal: &Principal,
    allow_subagents: bool,
) -> Result<Agent, AppError> {
    let context = state.context_for(principal);
    let tenant = principal.id.as_str();
    let session = cfg.session.as_str();
    #[rustfmt::skip]
    tracing::debug!(principal = tenant, session, "context: store resolved for principal");
    // The `docker` provider is resolved through the injected factory so tests can
    // substitute an in-process sandbox without a daemon. `browser` is resolved
    // here too: a `BrowserSandbox` (browser container manager + engine) is built
    // and wrapped as a `BrowserToolHandler`, while the agent's regular shell
    // sandbox stays the default Docker backend.
    let provider = SandboxProvider::parse(&cfg.sandbox_provider).ok_or_else(|| {
        AppError::Core(CoreError::Config(format!(
            "unknown sandbox: {} (the `codex` backend was removed; use docker/kata/cube/browser)",
            cfg.sandbox_provider
        )))
    })?;
    let (sandbox, mut handlers): (Arc<dyn Sandbox>, Vec<Arc<dyn agent_core::ToolHandler>>) =
        match provider {
            SandboxProvider::Docker => ((state.make_sandbox)(), Vec::new()),
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
    // Built-in tool families: every family talks to a capability seam, and the
    // fs/shell/jobs families are rooted at the single policy root. Families whose
    // seam is missing are simply not registered, so the model is never offered a
    // tool the deployment cannot serve (ADR-0016).
    let policy = agent_core::build_policy(&cfg)
        .map_err(AppError::Core)?
        .expect("policy on success");
    let set = aria_agent_tools::ToolSet::local(policy, sandbox.clone(), context.clone());
    handlers.extend(set.handlers());
    let mut tools = set.tools();
    tools.extend(agent_browser::browser_tools());
    // Orchestration families: only the ones this deployment can serve are
    // offered (ADR-0017). Sub-agents are backed by the in-process provider with a
    // cloud child-agent factory; a child itself runs with `allow_subagents =
    // false`, so the tool family disappears one level down.
    let subagents: Option<Arc<dyn Subagents>> = if allow_subagents {
        Some(Arc::new(InProcessSubagents::new(Arc::new(
            CloudSubagentsFactory {
                state: state.clone(),
                principal: principal.clone(),
                base: cfg.clone(),
            },
        ))))
    } else {
        None
    };
    let orchestrator = aria_agent_orchestrator::Orchestrator {
        subagents,
        teams: Some(state.teams.clone()),
        schedule: Some(state.schedule.clone()),
        owner: Some(principal.id.clone()),
    };
    handlers.extend(orchestrator.handlers());
    tools.extend(orchestrator.tools());

    Agent::with_handlers(cfg, (state.make_model)(), context, sandbox, handlers)
        // Session titles are projected into `session_titles` instead of a local
        // JSONL artifact: the cloud keeps its audit trail in Postgres.
        .map(|agent| {
            agent
                .with_tools(tools)
                .with_session_log(Arc::new(TitleProjection {
                    store: state.titles.clone(),
                    principal: principal.clone(),
                }))
        })
        .map_err(AppError::Core)
}

/// Runs one child agent through [`build_agent_inner`] (depth one).
struct CloudSubagentsFactory {
    state: AppState,
    principal: Principal,
    /// Parent config; only the session id / agent name change for the child.
    base: AgentConfig,
}

#[async_trait]
impl SubagentFactory for CloudSubagentsFactory {
    async fn run(&self, req: &SubagentRequest, session: &str) -> SeamResult<String> {
        let mut cfg = self.base.clone();
        cfg.session = session.to_string();
        cfg.agent_name = req
            .agent_name
            .clone()
            .unwrap_or_else(|| format!("{}-child", self.base.agent_name));
        // The child is bounded by the loop guard (`MAX_AGENTIC_STEPS`) rather than
        // `req.max_steps`: the loop owns its own iteration cap, and a silent
        // second cap that the provider cannot enforce would be a lie.
        let child = build_agent_inner(&self.state, cfg, &self.principal, false)
            .await
            .map_err(|e| SeamError::Io(format!("child agent: {e:?}")))?;
        child
            .run(&req.task)
            .await
            .map_err(|e| SeamError::Io(format!("child run: {e}")))
    }
}

/// Projects `SessionEvent::TitleSet` into the `session_titles` table.
///
/// Write-only on purpose: the stream stays the source of truth and titles are
/// read back through `GET /v1/agents/sessions/{id}/title` (ADR-0015).
struct TitleProjection {
    store: Arc<dyn TitleStore>,
    principal: Principal,
}

#[async_trait]
impl SessionPersistence for TitleProjection {
    async fn append(&self, session: &str, records: &[SessionRecord]) -> SessionResult<()> {
        for record in records {
            if let SessionEvent::TitleSet { title, source } = &record.event {
                // Each cloud turn builds a fresh `Agent`, so the deterministic
                // fallback is re-emitted every time. Once a title exists it must
                // not be replaced by a later first-prompt (the first input is the
                // session's identity); a provider override still wins.
                if source == SOURCE_FIRST_PROMPT {
                    let existing = self
                        .store
                        .get_title(&self.principal, session)
                        .await
                        .map_err(|e| SessionError::Io(format!("title projection: {e:?}")))?;
                    if existing.is_some() {
                        continue;
                    }
                }
                let row = TitleRow {
                    session_id: session.to_string(),
                    title: title.clone(),
                    source: source.clone(),
                    updated_at: record.at_ms,
                };
                self.store
                    .put_title(&self.principal, &row)
                    .await
                    // A projection failure keeps the batch so the next flush
                    // retries it instead of losing the title.
                    .map_err(|e| SessionError::Io(format!("title projection: {e:?}")))?;
            }
        }
        Ok(())
    }

    async fn read(&self, _session: &str, _from_seq: u64) -> SessionResult<Vec<SessionRecord>> {
        Err(SessionError::Io(
            "the cloud title projection is write-only; read titles from the session API".into(),
        ))
    }
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
        // Docker is the default sandbox backend (ADR-0003); kata/cube/browser
        // remain selectable per run.
        sandbox_provider: "docker".into(),
        model: DEFAULT_MODEL.into(),
        // The cloud keeps the memory graph off by default (its context store
        // supports it; enable per-deployment if desired). The SDK enables it.
        memory_graph: false,
        memory_compact_threshold: 0,
        // Deployment trust decision: the cloud has no interactive approval
        // channel, so tool calls are pre-approved by the deployment preset. With
        // an interactive channel a broker is injected instead (ADR-0016).
        approval_mode: "auto".into(),
        ..AgentConfig::default()
    }
}

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
            "/v1/agents/sessions/:session_id/title",
            get(api::sessions::get_session_title),
        )
        // --- orchestration: agent teams + scheduled jobs ---
        .route(
            "/v1/agent-teams",
            post(api::orchestration::create_team).get(api::orchestration::list_teams),
        )
        .route(
            "/v1/agent-teams/:team_id",
            get(api::orchestration::get_team),
        )
        .route(
            "/v1/agent-teams/:team_id/tasks/:task_id/claim",
            post(api::orchestration::claim_task),
        )
        .route(
            "/v1/agent-teams/:team_id/tasks/:task_id/complete",
            post(api::orchestration::complete_task),
        )
        .route(
            "/v1/scheduled-jobs",
            post(api::orchestration::create_job).get(api::orchestration::list_jobs),
        )
        .route(
            "/v1/scheduled-jobs/:job_id",
            delete(api::orchestration::delete_job),
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

/// The HTTP surface: the OpenAI beta Agents resource plus self-serve API keys.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_tests::{harness, harness_with};
    use crate::auth::{ApiKeyScopes, PrincipalKind};
    use agent_core::session::SessionError;

    fn principal() -> Principal {
        Principal {
            id: "tenant".into(),
            kind: PrincipalKind::User,
            scopes: ApiKeyScopes::read_write(),
        }
    }

    struct FailingTitles {
        fail_get: bool,
        fail_put: bool,
    }

    #[async_trait]
    impl TitleStore for FailingTitles {
        async fn put_title(&self, _p: &Principal, _row: &TitleRow) -> Result<(), AppError> {
            if self.fail_put {
                return Err(AppError::Core(CoreError::Config("put failed".into())));
            }
            Ok(())
        }
        async fn get_title(
            &self,
            _p: &Principal,
            _session: &str,
        ) -> Result<Option<TitleRow>, AppError> {
            if self.fail_get {
                return Err(AppError::Core(CoreError::Config("get failed".into())));
            }
            Ok(None)
        }
    }

    #[test]
    fn agent_tools_include_the_shell_and_browser_families() {
        let tools = agent_tools();
        assert!(tools.iter().any(|t| t.name == "shell"));
        assert!(tools.iter().any(|t| t.name.starts_with("browser_")));
    }

    #[tokio::test]
    async fn subagent_factory_reports_build_and_run_failures() {
        let h = harness();
        let bad_build = CloudSubagentsFactory {
            state: h.state.clone(),
            principal: principal(),
            base: AgentConfig {
                sandbox_provider: "bogus".into(),
                ..AgentConfig::default()
            },
        };
        let err = bad_build
            .run(
                &SubagentRequest {
                    parent_session: "parent".into(),
                    task: "hi".into(),
                    agent_name: None,
                    max_steps: 0,
                },
                "child",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("child agent"), "{err}");

        let h = harness_with(|| Box::new(crate::api_tests::FailingModel));
        let bad_run = CloudSubagentsFactory {
            state: h.state.clone(),
            principal: principal(),
            base: agent_config("parent", "", "parent"),
        };
        let err = bad_run
            .run(
                &SubagentRequest {
                    parent_session: "parent".into(),
                    task: "hi".into(),
                    agent_name: None,
                    max_steps: 0,
                },
                "child",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("child run"), "{err}");
    }

    #[tokio::test]
    async fn title_projection_surfaces_store_failures_and_is_write_only() {
        use agent_core::session::SessionPersistence;

        let record = |source: &str| SessionRecord {
            seq: 1,
            at_ms: 0,
            event: SessionEvent::TitleSet {
                title: "t".into(),
                source: source.into(),
            },
        };
        let projection = |fail_get: bool, fail_put: bool| TitleProjection {
            store: Arc::new(FailingTitles { fail_get, fail_put }),
            principal: principal(),
        };
        let err = projection(true, false)
            .append("s", &[record(SOURCE_FIRST_PROMPT)])
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::Io(_)), "{err:?}");
        let err = projection(false, true)
            .append("s", &[record("provider")])
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::Io(_)), "{err:?}");
        let err = projection(false, false).read("s", 0).await.unwrap_err();
        assert!(matches!(err, SessionError::Io(_)), "{err:?}");
    }
}
