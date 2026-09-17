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

use agent_core::context::{
    CompositeContextStore, ContextFragment, ContextStore, FragmentKind, MemoryBackend,
};
use agent_core::{Agent, AgentConfig, AgentEvent, Tool};
use agent_memo::MemoContextStore;
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
        rt().block_on(self.inner.run(&input))
            .map_err(|e| SdkError::Core(e.to_string()))
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
        rt().block_on(store.memorize(frag))
            .map_err(|e| SdkError::Core(e.to_string()))
    }

    /// Read a long-term memory by `key` (same `backend` override as
    /// [`SdkSession::memorize`]).
    pub fn recall(&self, key: String, backend: Option<String>) -> Result<Option<String>, SdkError> {
        let store = self.store_for(backend.as_deref())?;
        let frag = rt()
            .block_on(store.get_by_key(&self.session, &key))
            .map_err(|e| SdkError::Core(e.to_string()))?;
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
            }
            .map_err(|e| SdkError::Core(e.to_string()))?;
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
            Some(
                cloud_context::CloudContextStore::new(&base, key.as_deref())
                    .map_err(|e| SdkError::Core(e.to_string()))?,
            )
        }
    };

    let default: Arc<dyn ContextStore> = match (local.clone(), cloud.clone()) {
        (Some(l), Some(c)) => {
            if backend == MemoryBackend::Both {
                CompositeContextStore::new(l, c)
            } else {
                l
            }
        }
        (Some(l), None) => l,
        (None, Some(c)) => c,
        (None, None) => return Err(SdkError::Core("no memory backend configured".into())),
    };
    Ok((default, local, cloud, backend.as_str().to_string()))
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
    let cfg = AgentConfig {
        session: next_session(),
        agent_name,
        sandbox_provider,
        model,
        instructions,
    };
    let agent = Agent::new(cfg, context.clone()).map_err(|e| SdkError::Core(e.to_string()))?;
    Ok(Arc::new(SdkAgent {
        inner: Arc::new(agent),
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
/// `agent_tools()`.
fn sdk_tools() -> Vec<Tool> {
    vec![Tool {
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
    }]
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
            arguments: serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string()),
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
            let stream = agent
                .run_event_stream(&input, &tools)
                .await
                .map_err(|e| SdkError::Core(e.to_string()))?;
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
        match tc {
            SdkStreamEvent::ToolCall {
                id,
                name,
                arguments,
                result,
            } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "shell");
                assert_eq!(arguments, "{\"command\":[\"echo\",\"hi\"]}");
                assert_eq!(result, Some("hi\n".to_string()));
            }
            _ => panic!("expected ToolCall"),
        }
        assert!(matches!(
            map_event(&AgentEvent::Done { text: "bye".into() }),
            SdkStreamEvent::Done { text } if text == "bye"
        ));
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
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "shell");
        assert!(tools[0].parameters.is_object());
    }
}
