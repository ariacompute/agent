//! `ariacompute-agent` — the stable UniFFI boundary.
//!
//! This crate compiles to a native `cdylib` (`libaria-agent_ffi`) that Swift and
//! Kotlin consume through generated bindings (see `just ffi`). It wraps
//! `agent-core` (the codex-harness runtime) and `agent-memo`, exposing a small,
//! stable surface: [`SdkAgent`] and [`SdkSession`].
//!
//! FFI boundary stability rule (docs/adr/0002-ffi-boundary.md): any change here
//! must be followed by regenerating the Swift/Kotlin bindings and a CI check.

uniffi::setup_scaffolding!();

mod cloud_context;

use agent_browser::{BrowserCatalog, BrowserSandbox, BrowserToolHandler};
use agent_core::context::{
    CompositeContextStore, ContextFragment, ContextStore, FragmentKind, MemoryBackend,
};
use agent_core::{memo_tools, Agent, AgentConfig, AgentEvent, MemoToolHandler, Tool, ToolHandler};
use agent_memo::MemoContextStore;
use agent_sandbox::DockerSandbox;
use futures::StreamExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("tokio runtime"))
}

/// Process-unique, monotonically increasing session scope. The session is the
/// memo scoping key; it is auto-assigned per agent instance so callers never
/// have to pass one (mirrors the OpenAI Agents SDK quickstart, where
/// `Agent(name, model)` needs no session/thread).
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);
fn next_session() -> String {
    format!(
        "sess-{}-{}",
        std::process::id(),
        SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Where the agent's memory context lives.
///
/// `backend` is `cloud` | `local` | `both` (unknown values fall back to
/// `local`, the offline-first default for the native SDK).
#[derive(Clone, Debug, uniffi::Record)]
pub struct SdkMemoryConfig {
    /// `cloud` | `local` | `both`.
    pub backend: String,
    /// aria memo database path (empty ⇒ a temporary per-process database).
    pub local_db_path: String,
    /// agent-cloud base URL for the `cloud` / `both` backends
    /// (empty ⇒ `ARIA_AGENT_BASE_URL` or `http://localhost:3000`).
    pub cloud_base_url: String,
    /// API key for the cloud backend (empty ⇒ `ARIA_AGENT_API_KEY`).
    pub cloud_api_key: String,
}

impl Default for SdkMemoryConfig {
    fn default() -> Self {
        Self {
            backend: "local".to_string(),
            local_db_path: String::new(),
            cloud_base_url: String::new(),
            cloud_api_key: String::new(),
        }
    }
}

/// SDK-side agent configuration (advanced path). `session` is intentionally
/// absent: the runtime assigns one isolated context scope per agent instance.
///
/// Mirrors the OpenAI Agents SDK agent definition: name + instructions + model.
#[derive(Clone, Debug, uniffi::Record)]
pub struct SdkAgentConfig {
    pub agent_name: String,
    pub instructions: String,
    pub model: String,
    pub sandbox_provider: String,
    pub memory: SdkMemoryConfig,
}

impl Default for SdkAgentConfig {
    fn default() -> Self {
        Self {
            agent_name: "agent".to_string(),
            instructions: String::new(),
            sandbox_provider: "docker".to_string(),
            model: "gpt-4o-mini".to_string(),
            memory: SdkMemoryConfig::default(),
        }
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SdkError {
    #[error("core error: {0}")]
    Core(String),
}

// `From` conversions keep error propagation on `?` instead of per-call
// `map_err(|e| …)` closures (one shared, always-exercised line each).
impl From<agent_core::CoreError> for SdkError {
    fn from(e: agent_core::CoreError) -> Self {
        SdkError::Core(e.to_string())
    }
}

impl From<agent_core::context::ContextError> for SdkError {
    fn from(e: agent_core::context::ContextError) -> Self {
        SdkError::Core(e.to_string())
    }
}

/// A native agent handle. Construct via [`create_agent`].
#[derive(uniffi::Object)]
pub struct SdkAgent {
    inner: Arc<Agent>,
    context: Arc<dyn ContextStore>,
    local: Option<Arc<dyn ContextStore>>,
    cloud: Option<Arc<dyn ContextStore>>,
    backend: String,
}

#[uniffi::export]
impl SdkAgent {
    /// Run a single turn and return the assistant reply.
    pub fn run(&self, input: String) -> Result<String, SdkError> {
        Ok(rt().block_on(self.inner.run(&input))?)
    }

    /// Get the session memory handle for this agent.
    pub fn session(&self) -> Arc<SdkSession> {
        Arc::new(SdkSession {
            context: self.context.clone(),
            local: self.local.clone(),
            cloud: self.cloud.clone(),
            backend: self.backend.clone(),
            session: self.inner.session().to_string(),
        })
    }

    /// The memory backend this agent defaults to (`cloud` / `local` / `both`).
    pub fn memory_backend(&self) -> String {
        self.backend.clone()
    }

    /// The auto-assigned session id (context scope). Useful when relaying a run
    /// to the cloud `POST /v1/agents/sessions/{id}/events/stream` endpoint.
    pub fn session_id(&self) -> String {
        self.inner.session().to_string()
    }
}

/// A session's context handle (long-term / externalized memory).
///
/// The default backend comes from the agent's [`SdkMemoryConfig`]; `memorize` /
/// `recall` accept an optional `backend` (`cloud` / `local` / `both`) to
/// override it for a single call.
#[derive(uniffi::Object)]
pub struct SdkSession {
    /// The backend selected for the agent (`both` ⇒ composite store).
    context: Arc<dyn ContextStore>,
    local: Option<Arc<dyn ContextStore>>,
    cloud: Option<Arc<dyn ContextStore>>,
    backend: String,
    session: String,
}

impl SdkSession {
    /// Resolve a store, honouring an explicit `backend` override.
    fn store_for(&self, backend: Option<&str>) -> Result<Arc<dyn ContextStore>, SdkError> {
        let Some(raw) = backend.map(|b| b.trim()).filter(|b| !b.is_empty()) else {
            return Ok(self.context.clone());
        };
        let parsed: MemoryBackend = raw.parse().map_err(SdkError::Core)?;
        match parsed {
            MemoryBackend::Local => self
                .local
                .clone()
                .ok_or_else(|| SdkError::Core("local backend is not configured".into())),
            MemoryBackend::Cloud => self
                .cloud
                .clone()
                .ok_or_else(|| SdkError::Core("cloud backend is not configured".into())),
            MemoryBackend::Both => match (self.local.clone(), self.cloud.clone()) {
                (Some(l), Some(c)) => Ok(CompositeContextStore::new(l, c)),
                _ => Err(SdkError::Core(
                    "both backend needs local and cloud stores".into(),
                )),
            },
        }
    }
}

#[uniffi::export]
impl SdkSession {
    /// Write a long-term memory under `key`.
    ///
    /// `backend` (`cloud` / `local` / `both`) overrides the agent default for
    /// this call; pass `null` to use the default.
    pub fn memorize(
        &self,
        key: String,
        value: String,
        backend: Option<String>,
    ) -> Result<(), SdkError> {
        let store = self.store_for(backend.as_deref())?;
        let frag = ContextFragment::new(&self.session, FragmentKind::LongTerm, value).with_key(key);
        Ok(rt().block_on(store.memorize(frag))?)
    }

    /// Read a long-term memory by `key` (same `backend` override as
    /// [`SdkSession::memorize`]).
    pub fn recall(&self, key: String, backend: Option<String>) -> Result<Option<String>, SdkError> {
        let store = self.store_for(backend.as_deref())?;
        let frag = rt().block_on(store.get_by_key(&self.session, &key))?;
        Ok(frag.map(|f| f.content))
    }

    /// The backend this session defaults to (`cloud` / `local` / `both`).
    pub fn memory_backend(&self) -> String {
        self.backend.clone()
    }
}

/// Build the (default, local, cloud) stores for one agent.
///
/// * `local` — aria memo (SQLite): `local_db_path` → `ARIA_MEMO_DB` → temporary.
/// * `cloud` — agent-cloud REST: `cloud_base_url` / `cloud_api_key` →
///   `ARIA_AGENT_BASE_URL` / `ARIA_AGENT_API_KEY`.
#[allow(clippy::type_complexity)]
fn build_stores(
    memory: &SdkMemoryConfig,
) -> Result<
    (
        Arc<dyn ContextStore>,
        Option<Arc<dyn ContextStore>>,
        Option<Arc<dyn ContextStore>>,
        String,
    ),
    SdkError,
> {
    let backend: MemoryBackend = memory.backend.parse().unwrap_or(MemoryBackend::Local);

    let local: Option<Arc<dyn ContextStore>> = match backend {
        MemoryBackend::Cloud => None,
        _ => {
            let path = if memory.local_db_path.trim().is_empty() {
                std::env::var("ARIA_MEMO_DB").unwrap_or_default()
            } else {
                memory.local_db_path.clone()
            };
            let store = if path.trim().is_empty() {
                MemoContextStore::memory()
            } else {
                MemoContextStore::open(Path::new(&path))
            }?;
            Some(store)
        }
    };

    let cloud: Option<Arc<dyn ContextStore>> = match backend {
        MemoryBackend::Local => None,
        _ => {
            let base = if memory.cloud_base_url.trim().is_empty() {
                std::env::var("ARIA_AGENT_BASE_URL")
                    .unwrap_or_else(|_| "http://localhost:3000".into())
            } else {
                memory.cloud_base_url.clone()
            };
            let key = if memory.cloud_api_key.trim().is_empty() {
                std::env::var("ARIA_AGENT_API_KEY").ok()
            } else {
                Some(memory.cloud_api_key.clone())
            };
            Some(cloud_context::CloudContextStore::new(
                &base,
                key.as_deref(),
            )?)
        }
    };

    let default = pick_default(backend, local.clone(), cloud.clone())?;
    Ok((default, local, cloud, backend.as_str().to_string()))
}

/// Select the store an agent uses by default for `backend`.
///
/// Kept as a separate, total function so the (normally unreachable) mismatch
/// between the requested backend and the built stores is explicit rather than an
/// `unreachable!()` in the middle of `build_stores`.
fn pick_default(
    backend: MemoryBackend,
    local: Option<Arc<dyn ContextStore>>,
    cloud: Option<Arc<dyn ContextStore>>,
) -> Result<Arc<dyn ContextStore>, SdkError> {
    match (backend, local, cloud) {
        (MemoryBackend::Local, Some(local), _) => Ok(local),
        (MemoryBackend::Cloud, _, Some(cloud)) => Ok(cloud),
        (MemoryBackend::Both, Some(local), Some(cloud)) => {
            Ok(CompositeContextStore::new(local, cloud))
        }
        _ => Err(SdkError::Core(format!(
            "memory backend `{}` has no configured store",
            backend.as_str()
        ))),
    }
}

/// Build an [`SdkAgent`] with an auto-assigned, isolated session scope.
fn make_agent(
    agent_name: String,
    instructions: String,
    model: String,
    sandbox_provider: String,
    memory: SdkMemoryConfig,
) -> Result<Arc<SdkAgent>, SdkError> {
    let (context, local, cloud, backend) = build_stores(&memory)?;

    // The memory graph + auto-compact are the SDK's default (full integration);
    // core leaves them off so its own unit tests stay hermetic.
    let cfg = AgentConfig {
        session: next_session(),
        agent_name: agent_name.clone(),
        sandbox_provider: sandbox_provider.clone(),
        model: model.clone(),
        instructions: instructions.clone(),
        memory_graph: true,
        memory_compact_threshold: 32,
    };

    // Always wire the long-term-memory tool family as a `ToolHandler` so the agent
    // can actively read/write its own memory during a run. The `browser` sandbox is
    // an additional tool-handler family (not a shell sandbox): when selected we
    // build a `BrowserSandbox` (managing its own container) and inject it too,
    // while the agent's shell fallback stays on the regular Docker sandbox.
    let mut handlers: Vec<Arc<dyn ToolHandler>> =
        vec![Arc::new(MemoToolHandler::new(context.clone()))];
    if sandbox_provider.trim().eq_ignore_ascii_case("browser") {
        let browser = Arc::new(BrowserSandbox::with_catalog(
            Box::new(DockerSandbox::new()),
            BrowserCatalog::builtin(),
            BrowserCatalog::default_kind(),
        ));
        handlers.push(Arc::new(BrowserToolHandler::new(browser.clone())));
    }
    let sandbox: Arc<dyn agent_sandbox::Sandbox> = Arc::new(DockerSandbox::new());
    let model = agent_core::default_model(&cfg);
    let inner = Agent::with_handlers(cfg, model, context.clone(), sandbox, handlers)?;
    let inner: Arc<Agent> = Arc::new(inner);

    Ok(Arc::new(SdkAgent {
        inner,
        context,
        local,
        cloud,
        backend,
    }))
}

/// Minimal quickstart entry — `Agent(name, model)`, no session required.
///
/// The sandbox defaults to Docker and the memory backend to **local**
/// (aria memo). Streaming is via [`SdkAgent::run_stream`].
#[uniffi::export]
pub fn create_agent(agent_name: String, model: String) -> Result<Arc<SdkAgent>, SdkError> {
    make_agent(
        agent_name,
        String::new(),
        model,
        "docker".to_string(),
        SdkMemoryConfig::default(),
    )
}

/// Advanced entry accepting a full [`SdkAgentConfig`] (instructions, custom
/// sandbox, memory backend, …).
#[uniffi::export]
pub fn create_agent_with(config: SdkAgentConfig) -> Result<Arc<SdkAgent>, SdkError> {
    make_agent(
        config.agent_name,
        config.instructions,
        config.model,
        config.sandbox_provider,
        config.memory,
    )
}

/// Minimal tenant-scoped quickstart entry: the tenant id selects a separate
/// aria memo database (local backend). Empty `tenant_id` behaves like
/// [`create_agent`].
#[uniffi::export]
pub fn create_agent_for_tenant(
    tenant_id: String,
    agent_name: String,
    model: String,
) -> Result<Arc<SdkAgent>, SdkError> {
    let memory = tenant_memory(&tenant_id, SdkMemoryConfig::default());
    make_agent(
        agent_name,
        String::new(),
        model,
        "docker".to_string(),
        memory,
    )
}

/// Advanced tenant-scoped entry accepting a full [`SdkAgentConfig`].
#[uniffi::export]
pub fn create_agent_for_tenant_with(
    tenant_id: String,
    config: SdkAgentConfig,
) -> Result<Arc<SdkAgent>, SdkError> {
    let memory = tenant_memory(&tenant_id, config.memory.clone());
    make_agent(
        config.agent_name,
        config.instructions,
        config.model,
        config.sandbox_provider,
        memory,
    )
}

/// Scope a tenant to its own aria memo database when no explicit path was given
/// (`<ARIA_MEMO_DIR | tmp>/tenants/<tenant_id>/memo.db`).
fn tenant_memory(tenant_id: &str, memory: SdkMemoryConfig) -> SdkMemoryConfig {
    if tenant_id.trim().is_empty() || !memory.local_db_path.trim().is_empty() {
        return memory;
    }
    let base = std::env::var("ARIA_MEMO_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    SdkMemoryConfig {
        local_db_path: Path::new(&base)
            .join("tenants")
            .join(tenant_id)
            .join("memo.db")
            .to_string_lossy()
            .into_owned(),
        ..memory
    }
}

/// A single streaming event delivered to [`SdkAgentListener`] during
/// [`SdkAgent::run_stream`]. Mirrors the core [`AgentEvent`] so native apps can
/// render the agentic turn (phases, tool calls, streamed tokens, completion).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum SdkStreamEvent {
    /// A phase boundary (`recall` / `model` / `tool_exec` / `loop_guard`).
    Step {
        phase: String,
        label: Option<String>,
    },
    /// A streamed model text delta.
    Token { text: String },
    /// A tool invocation and its executed result (content only).
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        result: Option<String>,
    },
    /// Terminal event carrying the full final reply.
    Done { text: String },
    /// A stream error; the run terminates after this event.
    Error { message: String },
}

/// Receives streaming events from [`SdkAgent::run_stream`].
#[uniffi::export(callback_interface)]
pub trait SdkAgentListener: Send + Sync {
    fn on_event(&self, event: SdkStreamEvent);
}

/// The frozen toolset exposed by the SDK runtime: a single `shell` exec tool
/// whose `command` is a `[program, ...args]` array, matching the cloud's
/// `agent_tools()`. The browser tool family is appended (active only when the
/// agent is created with `sandbox_provider: "browser"`). The FFI export surface
/// is unchanged (ADR-0002): only this internal tool list grows.
fn sdk_tools() -> Vec<Tool> {
    let mut tools = vec![Tool {
        name: "shell".into(),
        description:
            "Execute a shell command given as a [program, ...args] array; runs in the configured sandbox."
                .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "command + args",
                }
            },
            "required": ["command"],
        }),
    }];
    tools.extend(agent_browser::browser_tools());
    tools.extend(memo_tools());
    tools
}

/// Translate a core [`AgentEvent`] into the SDK event shape. Pure and free of
/// FFI / network dependencies so it can be unit-tested in isolation.
pub fn map_event(ev: &AgentEvent) -> SdkStreamEvent {
    match ev {
        AgentEvent::Step { phase, label } => SdkStreamEvent::Step {
            phase: phase.clone(),
            label: label.clone(),
        },
        AgentEvent::Token { text } => SdkStreamEvent::Token { text: text.clone() },
        AgentEvent::ToolCall {
            id,
            name,
            arguments,
            result,
        } => SdkStreamEvent::ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: serde_json::to_string(arguments).unwrap_or_default(),
            result: Some(result.content.clone()),
        },
        AgentEvent::Done { text } => SdkStreamEvent::Done { text: text.clone() },
    }
}

#[uniffi::export]
impl SdkAgent {
    /// Run a single agentic turn, streaming events to `listener` as they arrive.
    /// The call blocks until the run terminates (a `Done` or `Error` event). The
    /// memo contract (recall before / persist both turns after) is honored by the
    /// underlying runtime.
    pub fn run_stream(
        &self,
        input: String,
        listener: Box<dyn SdkAgentListener>,
    ) -> Result<(), SdkError> {
        let agent = self.inner.clone();
        rt().block_on(async move {
            let tools = sdk_tools();
            let stream = agent.run_event_stream(&input, &tools).await?;
            let mut stream = stream;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(ev) => listener.on_event(map_event(&ev)),
                    Err(e) => {
                        listener.on_event(SdkStreamEvent::Error {
                            message: e.to_string(),
                        });
                        return Err(SdkError::Core(e.to_string()));
                    }
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_event_covers_every_variant() {
        assert!(matches!(
            map_event(&AgentEvent::Step { phase: "model".into(), label: Some("x".into()) }),
            SdkStreamEvent::Step { phase, label } if phase == "model" && label == Some("x".into())
        ));
        assert!(matches!(
            map_event(&AgentEvent::Token { text: "hi".into() }),
            SdkStreamEvent::Token { text } if text == "hi"
        ));
        let tc = map_event(&AgentEvent::ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({ "command": ["echo", "hi"] }),
            result: agent_core::ToolResult {
                call_id: "c1".into(),
                content: "hi\n".into(),
                is_error: false,
            },
        });
        assert!(matches!(tc, SdkStreamEvent::ToolCall { .. }));
        // Render instead of destructuring: an `if let` binding would leave the
        // non-matching arm as a permanently-uncovered line.
        let rendered = format!("{tc:?}");
        assert!(rendered.contains("\"c1\""), "{rendered}");
        assert!(rendered.contains("shell"), "{rendered}");
        assert!(rendered.contains("echo"), "{rendered}");
        assert!(rendered.contains("hi\\n"), "{rendered}");
        assert!(matches!(
            map_event(&AgentEvent::Done { text: "bye".into() }),
            SdkStreamEvent::Done { text } if text == "bye"
        ));
        let step = map_event(&AgentEvent::Step {
            phase: "recall".into(),
            label: None,
        });
        assert!(matches!(step, SdkStreamEvent::Step { label: None, .. }));
    }

    #[test]
    fn local_backend_is_the_default_and_roundtrips() {
        let (context, local, cloud, backend) = build_stores(&SdkMemoryConfig::default()).unwrap();
        assert_eq!(backend, "local");
        assert!(local.is_some() && cloud.is_none());
        let session = SdkSession {
            context,
            local,
            cloud,
            backend,
            session: "s".into(),
        };
        session
            .memorize("fact1".into(), "the moon is cheese".into(), None)
            .unwrap();
        assert_eq!(
            session.recall("fact1".into(), None).unwrap(),
            Some("the moon is cheese".into())
        );
        assert_eq!(session.memory_backend(), "local");
    }

    #[test]
    fn backend_override_errors_when_that_backend_is_not_configured() {
        let (context, local, cloud, backend) = build_stores(&SdkMemoryConfig::default()).unwrap();
        let session = SdkSession {
            context,
            local,
            cloud,
            backend,
            session: "s".into(),
        };
        // The agent is `local`-only, so a `cloud` override must fail loudly.
        let err = session
            .recall("fact1".into(), Some("cloud".into()))
            .expect_err("cloud backend is not configured");
        assert!(err.to_string().contains("cloud backend is not configured"));
        let err = session
            .recall("fact1".into(), Some("nonsense".into()))
            .expect_err("unknown backend");
        assert!(err.to_string().contains("unknown memory backend"));
    }

    #[test]
    fn both_backend_builds_local_and_cloud_without_connecting() {
        let dir = std::env::temp_dir().join(format!("aria-ffi-{}", next_session()));
        let cfg = SdkMemoryConfig {
            backend: "both".into(),
            local_db_path: dir.join("memo.db").to_string_lossy().into_owned(),
            cloud_base_url: "http://127.0.0.1:1".into(),
            cloud_api_key: String::new(),
        };
        let (_context, local, cloud, backend) = build_stores(&cfg).unwrap();
        assert_eq!(backend, "both");
        assert!(local.is_some() && cloud.is_some());
    }

    #[test]
    fn sdk_tools_is_the_shell_exec_tool() {
        let tools = sdk_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        // `shell` remains first; the browser and memory tool families are appended.
        assert!(
            names.len() >= 13,
            "expected shell + 8 browser + 4 memo tools, got {names:?}"
        );
        assert_eq!(names[0], "shell");
        for n in [
            "browser_navigate",
            "browser_extract",
            "browser_click",
            "browser_fill",
            "browser_screenshot",
            "browser_evaluate",
            "browser_solve_captcha",
            "browser_use",
        ] {
            assert!(names.contains(&n), "missing browser tool: {n}");
        }
        for n in ["memo_store", "memo_get", "memo_search", "memo_related"] {
            assert!(names.contains(&n), "missing memo tool: {n}");
        }
        assert!(tools[0].parameters.is_object());
    }

    // ------------------------------------------------------------ test support

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serialises env-var mutations and restores the previous process state.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn apply(vars: &[(&'static str, Option<&str>)]) -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let mut saved = Vec::new();
            for (key, value) in vars {
                saved.push((*key, std::env::var(key).ok()));
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self { _lock: lock, saved }
        }

        fn clear(keys: &[&'static str]) -> Self {
            let vars: Vec<(&'static str, Option<&str>)> = keys.iter().map(|k| (*k, None)).collect();
            Self::apply(&vars)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// A loopback OpenAI-compatible stub. Keeps its own multi-threaded runtime so
    /// the server keeps serving while the (blocking) SDK calls run on this thread.
    struct Gateway {
        rt: tokio::runtime::Runtime,
        server: wiremock::MockServer,
    }

    impl Gateway {
        fn start() -> Self {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let server = rt.block_on(wiremock::MockServer::start());
            Self { rt, server }
        }

        fn mount(&self, mock: wiremock::Mock) {
            self.rt.block_on(async { mock.mount(&self.server).await });
        }

        fn base_url(&self) -> String {
            format!("{}/v1", self.server.uri())
        }

        fn requests(&self) -> Vec<wiremock::Request> {
            self.rt
                .block_on(self.server.received_requests())
                .unwrap_or_default()
        }
    }

    fn completion(text: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 0,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": text },
            }],
        })
    }

    fn chat_mock(text: &str) -> wiremock::Mock {
        use wiremock::matchers::{method, path};
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(completion(text)))
            .expect(1)
    }

    fn failing_chat_mock() -> wiremock::Mock {
        use wiremock::matchers::{method, path};
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .expect(1)
    }

    /// Records the events a listener receives, rendered for assertions.
    struct Recorder(Arc<std::sync::Mutex<Vec<String>>>);

    impl SdkAgentListener for Recorder {
        fn on_event(&self, event: SdkStreamEvent) {
            let mut out = self.0.lock().unwrap();
            out.push(format!("{event:?}"));
        }
    }

    fn dummy_session(local: bool, cloud: bool) -> SdkSession {
        use agent_core::context::MemoryContextStore;
        SdkSession {
            context: MemoryContextStore::new(),
            local: local.then(|| MemoryContextStore::new() as Arc<dyn ContextStore>),
            cloud: cloud.then(|| MemoryContextStore::new() as Arc<dyn ContextStore>),
            backend: "both".into(),
            session: "s".into(),
        }
    }

    // -------------------------------------------------------------- configs

    #[test]
    fn sdk_config_defaults_are_the_quickstart_shape() {
        let memory = SdkMemoryConfig::default();
        assert_eq!(memory.backend, "local");
        assert!(memory.local_db_path.is_empty());
        assert!(memory.cloud_base_url.is_empty());
        assert!(memory.cloud_api_key.is_empty());

        let config = SdkAgentConfig::default();
        assert_eq!(config.agent_name, "agent");
        assert!(config.instructions.is_empty());
        assert_eq!(config.sandbox_provider, "docker");
        assert_eq!(config.model, "gpt-4o-mini");
        assert_eq!(config.memory.backend, "local");
    }

    #[test]
    fn tenant_memory_keeps_explicit_or_empty_scopes() {
        // No tenant, or an explicit path, means "leave the config alone".
        let untouched = tenant_memory("", SdkMemoryConfig::default());
        assert!(untouched.local_db_path.is_empty());
        let explicit = SdkMemoryConfig {
            local_db_path: "/tmp/elsewhere.db".into(),
            ..SdkMemoryConfig::default()
        };
        assert_eq!(
            tenant_memory("acme", explicit).local_db_path,
            "/tmp/elsewhere.db"
        );
    }

    #[test]
    fn tenant_memory_scopes_the_default_database_under_tenants() {
        let dir = std::env::temp_dir().join(format!("aria-tenant-{}", next_session()));
        let env = EnvGuard::apply(&[("ARIA_MEMO_DIR", Some(dir.to_string_lossy().as_ref()))]);
        let scoped = tenant_memory("acme", SdkMemoryConfig::default());
        drop(env);
        assert_eq!(
            scoped.local_db_path,
            dir.join("tenants")
                .join("acme")
                .join("memo.db")
                .to_string_lossy()
        );
    }

    #[test]
    fn tenant_memory_falls_back_to_the_temp_dir() {
        let env = EnvGuard::clear(&["ARIA_MEMO_DIR"]);
        let scoped = tenant_memory("acme", SdkMemoryConfig::default());
        drop(env);
        // Compare whole paths, not the rendered string: the separator differs
        // between Windows (`\`) and Unix (`/`).
        let expected = std::env::temp_dir()
            .join("tenants")
            .join("acme")
            .join("memo.db");
        assert_eq!(std::path::PathBuf::from(&scoped.local_db_path), expected);
    }

    #[test]
    fn tenant_entries_build_agents() {
        let dir = std::env::temp_dir().join(format!("aria-tenant-entry-{}", next_session()));
        let _env = EnvGuard::apply(&[("ARIA_MEMO_DIR", Some(dir.to_string_lossy().as_ref()))]);

        let agent = create_agent_for_tenant("acme".into(), "t".into(), "gpt-test".into()).unwrap();
        assert_eq!(agent.memory_backend(), "local");
        assert!(dir.join("tenants").join("acme").join("memo.db").exists());

        let config = SdkAgentConfig {
            agent_name: "t2".into(),
            instructions: "be nice".into(),
            model: "gpt-test".into(),
            sandbox_provider: "browser".into(),
            memory: SdkMemoryConfig {
                local_db_path: dir.join("t2.db").to_string_lossy().into_owned(),
                ..SdkMemoryConfig::default()
            },
        };
        let agent = create_agent_for_tenant_with("acme".into(), config).unwrap();
        assert_eq!(agent.memory_backend(), "local");
        assert!(dir.join("t2.db").exists());

        // An empty tenant behaves like `create_agent` (no tenant directory).
        let agent = create_agent_for_tenant(String::new(), "t3".into(), "gpt-test".into()).unwrap();
        assert_eq!(agent.memory_backend(), "local");
    }

    // --------------------------------------------------------------- backends

    #[test]
    fn store_for_resolves_overrides_and_reports_missing_stores() {
        let both = dummy_session(true, true);
        assert!(both.store_for(None).is_ok());
        assert!(both.store_for(Some("   ")).is_ok());
        assert!(both.store_for(Some("local")).is_ok());
        assert!(both.store_for(Some("cloud")).is_ok());
        assert!(both.store_for(Some("both")).is_ok());
        let err = both.store_for(Some("nonsense")).err().unwrap();
        assert!(err.to_string().contains("unknown memory backend"), "{err}");

        let local_only = dummy_session(true, false);
        let err = local_only.store_for(Some("cloud")).err().unwrap();
        assert!(
            err.to_string().contains("cloud backend is not configured"),
            "{err}"
        );
        let err = local_only.store_for(Some("both")).err().unwrap();
        assert!(
            err.to_string()
                .contains("both backend needs local and cloud stores"),
            "{err}"
        );

        let cloud_only = dummy_session(false, true);
        let err = cloud_only.store_for(Some("local")).err().unwrap();
        assert!(
            err.to_string().contains("local backend is not configured"),
            "{err}"
        );
    }

    #[test]
    fn pick_default_rejects_a_backend_without_its_store() {
        use agent_core::context::MemoryContextStore;
        assert!(pick_default(MemoryBackend::Local, None, None).is_err());
        assert!(pick_default(MemoryBackend::Cloud, Some(MemoryContextStore::new()), None).is_err());
        assert!(pick_default(MemoryBackend::Both, Some(MemoryContextStore::new()), None).is_err());
        let err = pick_default(MemoryBackend::Local, None, None)
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .contains("memory backend `local` has no configured store"),
            "{err}"
        );
        assert!(pick_default(MemoryBackend::Local, Some(MemoryContextStore::new()), None).is_ok());
        assert!(pick_default(MemoryBackend::Cloud, None, Some(MemoryContextStore::new())).is_ok());
        assert!(pick_default(
            MemoryBackend::Both,
            Some(MemoryContextStore::new()),
            Some(MemoryContextStore::new())
        )
        .is_ok());
    }

    #[test]
    fn cloud_backend_uses_env_fallbacks_and_skips_the_local_store() {
        let _env = EnvGuard::clear(&["ARIA_AGENT_BASE_URL", "ARIA_AGENT_API_KEY", "ARIA_MEMO_DB"]);
        let cfg = SdkMemoryConfig {
            backend: "cloud".into(),
            ..SdkMemoryConfig::default()
        };
        let (_context, local, cloud, backend) = build_stores(&cfg).unwrap();
        assert!(local.is_none(), "cloud-only agents must not open a memo db");
        assert!(cloud.is_some());
        assert_eq!(backend, "cloud");
    }

    #[test]
    fn explicit_cloud_settings_win_over_the_environment() {
        let _env = EnvGuard::apply(&[
            ("ARIA_AGENT_BASE_URL", Some("http://127.0.0.1:1")),
            ("ARIA_AGENT_API_KEY", Some("env-key")),
        ]);
        let cfg = SdkMemoryConfig {
            backend: "both".into(),
            local_db_path: String::new(),
            cloud_base_url: "http://127.0.0.1:2".into(),
            cloud_api_key: "explicit-key".into(),
        };
        let (context, local, cloud, backend) = build_stores(&cfg).unwrap();
        assert!(local.is_some() && cloud.is_some());
        assert_eq!(backend, "both");
        // The composite store is the default for `both`.
        assert!(Arc::ptr_eq(&context, &context));
    }

    #[test]
    fn build_stores_reports_unusable_paths_and_urls() {
        let _env = EnvGuard::apply(&[("ARIA_MEMO_DB", None), ("ARIA_AGENT_BASE_URL", None)]);

        // A database path under a regular file can never be opened. A temp-dir
        // blocker (not `/dev/null`) keeps this true on Windows as well.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let bad_path = SdkMemoryConfig {
            local_db_path: blocker.join("memo.db").to_string_lossy().to_string(),
            ..SdkMemoryConfig::default()
        };
        let err = build_stores(&bad_path).err().unwrap();
        assert!(err.to_string().starts_with("core error:"), "{err}");

        // An explicitly blank base URL is rejected by the cloud store.
        std::env::set_var("ARIA_AGENT_BASE_URL", "");
        let blank_url = SdkMemoryConfig {
            backend: "cloud".into(),
            ..SdkMemoryConfig::default()
        };
        let err = build_stores(&blank_url).err().unwrap();
        assert!(err.to_string().contains("cloud base url is empty"), "{err}");
    }

    #[test]
    fn unknown_backend_falls_back_to_local() {
        let cfg = SdkMemoryConfig {
            backend: "satellite".into(),
            ..SdkMemoryConfig::default()
        };
        let (_context, local, cloud, backend) = build_stores(&cfg).unwrap();
        assert_eq!(backend, "local");
        assert!(local.is_some() && cloud.is_none());
    }

    #[test]
    fn session_surfaces_store_errors() {
        let cloud = crate::cloud_context::CloudContextStore::new("http://127.0.0.1:0", None)
            .unwrap() as Arc<dyn ContextStore>;
        let session = SdkSession {
            context: cloud.clone(),
            local: None,
            cloud: Some(cloud),
            backend: "cloud".into(),
            session: "s".into(),
        };
        let err = session
            .memorize("k".into(), "v".into(), None)
            .expect_err("cloud is unreachable");
        assert!(err.to_string().contains("cloud write failed"), "{err}");
        let err = session
            .recall("k".into(), None)
            .expect_err("cloud is unreachable");
        assert!(err.to_string().contains("cloud read failed"), "{err}");
    }

    // ------------------------------------------------------------ agent runs

    #[test]
    fn quickstart_agent_runs_a_turn_against_the_gateway() {
        let gateway = Gateway::start();
        gateway.mount(chat_mock("hi there"));
        let base = gateway.base_url();
        let _env = EnvGuard::apply(&[
            ("GATEWAY_BASE_URL", Some(base.as_str())),
            ("GATEWAY_API_KEY", Some("test-key")),
        ]);

        let agent = create_agent("quickstart".into(), "gpt-test".into()).unwrap();
        assert_eq!(agent.memory_backend(), "local");
        assert!(agent.session_id().starts_with("sess-"));
        assert_eq!(agent.run("hello".into()).unwrap(), "hi there");

        let session = agent.session();
        assert_eq!(session.memory_backend(), "local");
        session
            .memorize("fact".into(), "water is wet".into(), None)
            .unwrap();
        assert_eq!(
            session.recall("fact".into(), None).unwrap(),
            Some("water is wet".into())
        );

        let reqs = gateway.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].url.path(), "/v1/chat/completions");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.ends_with("USER: hello"), "{user}");
        assert_eq!(body["model"], "gpt-test");
    }

    #[test]
    fn run_surfaces_model_errors() {
        let gateway = Gateway::start();
        gateway.mount(failing_chat_mock());
        let base = gateway.base_url();
        let _env = EnvGuard::apply(&[
            ("GATEWAY_BASE_URL", Some(base.as_str())),
            ("GATEWAY_API_KEY", Some("test-key")),
        ]);
        let agent = create_agent("failing".into(), "gpt-test".into()).unwrap();
        let err = agent.run("hello".into()).unwrap_err();
        assert!(err.to_string().starts_with("core error:"), "{err}");
    }

    #[test]
    fn run_stream_emits_events_until_done() {
        let gateway = Gateway::start();
        gateway.mount(chat_mock("streamed reply"));
        let base = gateway.base_url();
        let _env = EnvGuard::apply(&[
            ("GATEWAY_BASE_URL", Some(base.as_str())),
            ("GATEWAY_API_KEY", Some("test-key")),
        ]);

        let agent = create_agent("streamer".into(), "gpt-test".into()).unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        agent
            .run_stream("hello".into(), Box::new(Recorder(events.clone())))
            .unwrap();

        let seen = events.lock().unwrap().clone();
        assert_eq!(
            seen.first().map(String::as_str),
            Some("Step { phase: \"recall\", label: None }")
        );
        assert!(
            seen.contains(&"Step { phase: \"model\", label: None }".to_string()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&"Token { text: \"streamed reply\" }".to_string()),
            "{seen:?}"
        );
        assert_eq!(
            seen.last().map(String::as_str),
            Some("Done { text: \"streamed reply\" }")
        );
    }

    #[test]
    fn run_stream_reports_stream_errors() {
        let gateway = Gateway::start();
        gateway.mount(failing_chat_mock());
        let base = gateway.base_url();
        let _env = EnvGuard::apply(&[
            ("GATEWAY_BASE_URL", Some(base.as_str())),
            ("GATEWAY_API_KEY", Some("test-key")),
        ]);

        let agent = create_agent("streamer".into(), "gpt-test".into()).unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let err = agent
            .run_stream("hello".into(), Box::new(Recorder(events.clone())))
            .unwrap_err();
        assert!(err.to_string().starts_with("core error:"), "{err}");
        let seen = events.lock().unwrap().clone();
        assert!(
            seen.last()
                .is_some_and(|e| e.starts_with("Error { message:")),
            "{seen:?}"
        );
    }

    #[test]
    fn run_stream_propagates_a_failing_context() {
        // An unreachable cloud store fails during the recall that precedes the
        // model call, so `run_event_stream` itself returns an error.
        let context: Arc<dyn ContextStore> =
            crate::cloud_context::CloudContextStore::new("http://127.0.0.1:0", None).unwrap();
        let cfg = AgentConfig {
            session: "unreachable".into(),
            ..AgentConfig::default()
        };
        let model = agent_core::default_model(&cfg);
        let sandbox: Arc<dyn agent_sandbox::Sandbox> = Arc::new(DockerSandbox::new());
        let inner = Agent::with_handlers(cfg, model, context.clone(), sandbox, vec![]).unwrap();

        let agent = SdkAgent {
            inner: Arc::new(inner),
            context,
            local: None,
            cloud: None,
            backend: "cloud".into(),
        };
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let err = agent
            .run_stream("hello".into(), Box::new(Recorder(events.clone())))
            .unwrap_err();
        assert!(err.to_string().contains("cloud request failed"), "{err}");
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn advanced_entry_builds_an_agent_with_instructions_and_a_browser() {
        let config = SdkAgentConfig {
            agent_name: "advanced".into(),
            instructions: "be concise".into(),
            model: "gpt-test".into(),
            sandbox_provider: "browser".into(),
            memory: SdkMemoryConfig::default(),
        };
        let agent = create_agent_with(config).unwrap();
        assert_eq!(agent.memory_backend(), "local");
        assert!(agent.session_id().starts_with("sess-"));
    }

    #[test]
    fn env_guard_restores_previous_values() {
        // A pre-existing value exercises the `Some` restore arm; a variable that
        // was absent exercises the `None` arm. Guards are never nested (the lock
        // is not re-entrant).
        const KEY: &str = "ARIA_AGENT_TEST_SEED";
        std::env::set_var(KEY, "first");
        let guard = EnvGuard::apply(&[(KEY, Some("second"))]);
        assert_eq!(std::env::var(KEY).unwrap(), "second");
        drop(guard);
        assert_eq!(std::env::var(KEY).unwrap(), "first");

        std::env::remove_var(KEY);
        let guard = EnvGuard::clear(&[KEY]);
        assert!(std::env::var(KEY).is_err());
        drop(guard);
        assert!(std::env::var(KEY).is_err());
    }
}

/// End-to-end integration for the memo integration: a real `MemoContextStore`
/// (aria memo, SQLite) backs the agent, which must (a) recall across turns,
/// (b) build memory-graph edges between turns, and (c) compact without unbounded
/// growth. Mirrors the SDK's default config (`memory_graph = true`, threshold 32).
#[cfg(test)]
mod memo_integration_tests {
    use agent_core::context::{ContextStore, RecallQuery};
    use agent_core::{
        Agent as CoreAgent, AgentConfig as CoreAgentConfig, MemoToolHandler, ModelClient,
        StubModel, ToolCall, ToolHandler,
    };
    use agent_memo::MemoContextStore;
    use agent_sandbox::DockerSandbox;
    use serde_json::json;
    use std::sync::Arc;

    fn make_agent(
        session: &str,
        graph: bool,
        threshold: usize,
    ) -> (Arc<CoreAgent>, Arc<MemoContextStore>) {
        let memo = MemoContextStore::memory().unwrap();
        let cfg = CoreAgentConfig {
            session: session.to_string(),
            memory_graph: graph,
            memory_compact_threshold: threshold,
            ..Default::default()
        };
        let sandbox = Arc::new(DockerSandbox::new());
        let model: Box<dyn ModelClient> = Box::new(StubModel::new("agent"));
        let handler: Arc<dyn ToolHandler> = Arc::new(MemoToolHandler::new(memo.clone()));
        let agent = Arc::new(
            CoreAgent::with_handlers(cfg, model, memo.clone(), sandbox, vec![handler]).unwrap(),
        );
        (agent, memo)
    }

    #[tokio::test]
    async fn cross_turn_recall_and_graph_edges() {
        let (agent, memo) = make_agent("memo-int", true, 1000);

        // First turn plants a fact; second turn should recall it via vector recall
        // (and the graph expansion seeded from the recalled fragment).
        let _ = agent.run("remember: the sky is blue").await.unwrap();
        let _ = agent.run("what color is the sky?").await.unwrap();

        let frags = memo
            .recall(&RecallQuery::new("memo-int", "sky blue"))
            .await
            .unwrap();
        assert!(
            frags.iter().any(|f| f.content.contains("sky is blue")),
            "expected cross-turn recall of the planted fact"
        );

        // The run loop must have related the user turn to the assistant turn.
        let rels = memo
            .get_relations("memo-int", None, None, None, 50)
            .await
            .unwrap();
        assert!(!rels.is_empty(), "expected memory-graph edges after turns");
    }

    #[tokio::test]
    async fn memo_tools_persist_then_query() {
        let memo = MemoContextStore::memory().unwrap();
        let handler = MemoToolHandler::new(memo.clone());

        let store = handler
            .run(
                &ToolCall {
                    id: "1".into(),
                    name: "memo_store".into(),
                    arguments: json!({ "key": "pref", "value": "likes rust" }),
                },
                &(memo.clone() as Arc<dyn ContextStore>),
                "s",
            )
            .await;
        assert!(!store.is_error, "memo_store failed: {}", store.content);

        let got = handler
            .run(
                &ToolCall {
                    id: "2".into(),
                    name: "memo_get".into(),
                    arguments: json!({ "key": "pref" }),
                },
                &(memo.clone() as Arc<dyn ContextStore>),
                "s",
            )
            .await;
        assert!(got.content.contains("likes rust"), "got: {}", got.content);
    }

    #[tokio::test]
    async fn compact_is_idempotent_and_bounded() {
        let (agent, memo) = make_agent("memo-compact", true, 4);

        for i in 0..5 {
            let _ = agent.run(&format!("turn {i}")).await.unwrap();
        }

        // The compaction note is idempotent: repeated triggers reuse the same id
        // instead of accumulating a new note each turn.
        let compact = memo
            .get_by_key("memo-compact", "__compact__memo-compact")
            .await
            .unwrap();
        assert!(
            compact.is_some(),
            "expected a single idempotent compaction note"
        );

        let all = memo.list_session("memo-compact", usize::MAX).await.unwrap();
        let notes = all
            .iter()
            .filter(|f| f.key.as_deref() == Some("__compact__memo-compact"))
            .count();
        assert_eq!(notes, 1, "compaction note must be idempotent (got {notes})");
    }
}
