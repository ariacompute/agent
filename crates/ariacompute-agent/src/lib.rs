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

use agent_core::{Agent, AgentConfig, AgentEvent, Tool};
use agent_memo::{ContextFragment, FragmentKind, MemoStore, SledMemoStore};
use futures::StreamExt;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("tokio runtime"))
}

/// SDK-side agent configuration (mirrors `agent_core::AgentConfig`).
#[derive(Clone, Debug, uniffi::Record)]
pub struct SdkAgentConfig {
    pub session: String,
    pub agent_name: String,
    pub sandbox_provider: String,
    pub model: String,
}

impl Default for SdkAgentConfig {
    fn default() -> Self {
        Self {
            session: "default".to_string(),
            agent_name: "agent".to_string(),
            sandbox_provider: "docker".to_string(),
            model: "gpt-4o-mini".to_string(),
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
            memo: self.inner.memo(),
            session: self.inner.session().to_string(),
        })
    }
}

/// A session's memo handle (long-term / externalized memory).
#[derive(uniffi::Object)]
pub struct SdkSession {
    memo: Arc<dyn MemoStore>,
    session: String,
}

#[uniffi::export]
impl SdkSession {
    /// Write a long-term memory under `key`.
    pub fn memorize(&self, key: String, value: String) -> Result<(), SdkError> {
        let frag = ContextFragment::new(&self.session, FragmentKind::LongTerm, value).with_key(key);
        rt().block_on(self.memo.memorize(frag))
            .map_err(|e| SdkError::Core(e.to_string()))
    }

    /// Read a long-term memory by `key`.
    pub fn recall(&self, key: String) -> Result<Option<String>, SdkError> {
        let frag = rt()
            .block_on(self.memo.get_by_key(&self.session, &key))
            .map_err(|e| SdkError::Core(e.to_string()))?;
        Ok(frag.map(|f| f.content))
    }
}

/// Build an agent with an in-memory memo store.
#[uniffi::export]
pub fn create_agent(config: SdkAgentConfig) -> Result<Arc<SdkAgent>, SdkError> {
    let memo = SledMemoStore::memory().map_err(|e| SdkError::Core(e.to_string()))?;
    let cfg = AgentConfig {
        session: config.session,
        agent_name: config.agent_name,
        sandbox_provider: config.sandbox_provider,
        model: config.model,
    };
    let agent = Agent::new(cfg, memo).map_err(|e| SdkError::Core(e.to_string()))?;
    Ok(Arc::new(SdkAgent {
        inner: Arc::new(agent),
    }))
}

/// Build an agent whose memo store is namespaced to `tenant_id`, giving the
/// native SDK the same per-tenant isolation the cloud enforces server-side.
/// When `tenant_id` is empty behavior matches [`create_agent`] (in-memory memo).
#[uniffi::export]
pub fn create_agent_for_tenant(
    tenant_id: String,
    config: SdkAgentConfig,
) -> Result<Arc<SdkAgent>, SdkError> {
    let memo: Arc<dyn MemoStore> = if tenant_id.is_empty() {
        SledMemoStore::memory().map_err(|e| SdkError::Core(e.to_string()))?
    } else {
        // Persistent per-tenant sled DB under ARIA_MEMO_DIR (or a temp subdir).
        let base = std::env::var("ARIA_MEMO_DIR")
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        let dir = Path::new(&base).join("tenants").join(&tenant_id);
        std::fs::create_dir_all(&dir).map_err(|e| SdkError::Core(e.to_string()))?;
        SledMemoStore::open(&dir).map_err(|e| SdkError::Core(e.to_string()))?
    };
    let cfg = AgentConfig {
        session: config.session,
        agent_name: config.agent_name,
        sandbox_provider: config.sandbox_provider,
        model: config.model,
    };
    let agent = Agent::new(cfg, memo).map_err(|e| SdkError::Core(e.to_string()))?;
    Ok(Arc::new(SdkAgent {
        inner: Arc::new(agent),
    }))
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
    fn sdk_tools_is_the_shell_exec_tool() {
        let tools = sdk_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "shell");
        assert!(tools[0].parameters.is_object());
    }
}
